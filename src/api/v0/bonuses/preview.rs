use std::{
	collections::{BTreeSet, HashMap, HashSet},
	time::Duration,
};

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::{HeaderValue, StatusCode, header},
	response::{IntoResponse, Response},
};
use chrono::Utc;
use entities::{
	cosmetic, cosmetic_allowed_slot, player_owned_cosmetic, prelude::*,
	sea_orm_active_enums::BodySlot,
};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Select,
	sea_query::Query,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{BonusRef, catalog};
use crate::api::{
	ApiState, error_response,
	permissions::{Require, perm},
	rate_limit::RateLimiter,
	v0::{account::AuthenticatedPlayer, cosmetics::in_enabled_group},
};

pub(crate) const MAX_PREVIEW_ITEMS: usize = 32;

#[derive(thiserror::Error, Debug, OperationIo)]
pub(crate) enum PreviewError {
	#[error("Too many cosmetics in one preview (max {MAX_PREVIEW_ITEMS})")]
	TooManyItems,
	#[error("Cosmetic {0} can't be worn")]
	NotWearable(i32),
	#[error("These cosmetics can't be worn together")]
	SlotConflict,
	#[error("Too many requests, please slow down")]
	RateLimited(Duration),
	#[error("Bonuses are unavailable right now")]
	Unavailable,
}

impl IntoResponse for PreviewError {
	fn into_response(self) -> Response {
		let status = match self {
			Self::TooManyItems | Self::NotWearable(_) | Self::SlotConflict => {
				StatusCode::BAD_REQUEST
			}
			Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
			Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
		};
		let retry_after = match self {
			Self::RateLimited(wait) => {
				HeaderValue::from_str(&wait.as_secs().max(1).to_string()).ok()
			}
			_ => None,
		};
		let mut response = error_response(status, self);
		if let Some(retry_after) = retry_after {
			response
				.headers_mut()
				.insert(header::RETRY_AFTER, retry_after);
		}
		response
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct PreviewRequest {
	cosmetic_ids: Vec<i32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PreviewResponse {
	bonuses: Vec<BonusRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Viewer {
	Player(i32),
	Admin,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("previewBonuses")
		.summary("Preview the bonuses of an outfit")
		.description(
			"Returns the bonus effects the given cosmetics would give if worn \
			 together. They must fit one per slot, as when equipping, and each \
			 must be in the catalogue or owned by the player. Nothing is \
			 recorded, and the request is rate limited per player.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"Too many cosmetics in one request, or cosmetics that can't be worn \
				 together",
			)
		})
		.response_with::<{ StatusCode::TOO_MANY_REQUESTS.as_u16() }, String, _>(|res| {
			res.description("Too many previews; wait for the Retry-After header")
		})
}

pub(crate) fn admin_preview_doc(op: TransformOperation) -> TransformOperation {
	op.id("adminPreviewBonuses")
		.summary("Test which sets an outfit completes")
		.description(
			"Returns the bonus effects the given cosmetics would give if worn \
			 together, resolved like the player preview. Hidden cosmetics and \
			 cosmetics in hidden groups count, and nobody has to own them, so \
			 sets can be tested before their cosmetics go live. They must still \
			 fit one per slot. Nothing is recorded and there is no rate limit. \
			 Requires `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"Too many cosmetics in one request, or cosmetics that can't be worn \
				 together",
			)
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/preview", post_with(self::endpoint, self::endpoint_doc))
}

fn check_size(cosmetic_ids: &[i32]) -> Result<(), PreviewError> {
	if cosmetic_ids.len() > MAX_PREVIEW_ITEMS {
		return Err(PreviewError::TooManyItems);
	}
	Ok(())
}

pub(crate) async fn check_preview(
	limiter: &RateLimiter<i32>,
	player_id: i32,
	cosmetic_ids: &[i32],
) -> Result<(), PreviewError> {
	if let Some(wait) = limiter.check(player_id).await {
		return Err(PreviewError::RateLimited(wait));
	}
	check_size(cosmetic_ids)
}

fn wearable_query(viewer: Viewer, cosmetic_ids: &[i32]) -> Select<CosmeticAllowedSlot> {
	let query = CosmeticAllowedSlot::find()
		.inner_join(Cosmetic)
		.filter(cosmetic_allowed_slot::Column::CosmeticId.is_in(cosmetic_ids.iter().copied()));
	let Viewer::Player(player_id) = viewer else {
		return query;
	};
	let owned = Query::select()
		.column(player_owned_cosmetic::Column::CosmeticId)
		.from(player_owned_cosmetic::Entity)
		.and_where(player_owned_cosmetic::Column::PlayerId.eq(player_id))
		.to_owned();
	query.filter(
		Condition::any()
			.add(
				Condition::all()
					.add(cosmetic::Column::Enabled.eq(true))
					.add(in_enabled_group()),
			)
			.add(cosmetic::Column::Id.in_subquery(owned)),
	)
}

async fn wearable_slots(
	db: &impl ConnectionTrait,
	viewer: Viewer,
	cosmetic_ids: &[i32],
) -> Result<HashMap<i32, Vec<BodySlot>>, DbErr> {
	let rows = wearable_query(viewer, cosmetic_ids).all(db).await?;

	let mut slots: HashMap<i32, Vec<BodySlot>> = HashMap::new();
	for row in rows {
		slots.entry(row.cosmetic_id).or_default().push(row.slot);
	}
	Ok(slots)
}

fn claim_slot(
	item: usize,
	allowed: &[&[BodySlot]],
	holders: &mut HashMap<BodySlot, usize>,
	tried: &mut HashSet<BodySlot>,
) -> bool {
	for slot in allowed[item] {
		if !tried.insert(slot.clone()) {
			continue;
		}
		let free = match holders.get(slot) {
			None => true,
			Some(&holder) => claim_slot(holder, allowed, holders, tried),
		};
		if free {
			holders.insert(slot.clone(), item);
			return true;
		}
	}
	false
}

pub(crate) fn check_loadout(
	cosmetic_ids: &[i32],
	slots: &HashMap<i32, Vec<BodySlot>>,
) -> Result<(), PreviewError> {
	let ids: BTreeSet<i32> = cosmetic_ids.iter().copied().collect();
	let allowed = ids
		.iter()
		.map(|id| match slots.get(id) {
			Some(allowed) if !allowed.is_empty() => Ok(allowed.as_slice()),
			_ => Err(PreviewError::NotWearable(*id)),
		})
		.collect::<Result<Vec<_>, _>>()?;

	let mut holders = HashMap::new();
	for item in 0..allowed.len() {
		if !claim_slot(item, &allowed, &mut holders, &mut HashSet::new()) {
			return Err(PreviewError::SlotConflict);
		}
	}
	Ok(())
}

async fn preview_outfit(
	state: &ApiState,
	viewer: Viewer,
	cosmetic_ids: Vec<i32>,
) -> Result<PreviewResponse, PreviewError> {
	if cosmetic_ids.is_empty() {
		return Ok(PreviewResponse {
			bonuses: Vec::new(),
		});
	}

	let slots = wearable_slots(&state.database, viewer, &cosmetic_ids)
		.await
		.map_err(|error| {
			warn!(%error, "Unable to load cosmetic slots for a bonus preview");
			PreviewError::Unavailable
		})?;
	check_loadout(&cosmetic_ids, &slots)?;

	let catalog = catalog(state).await.ok_or(PreviewError::Unavailable)?;
	let bonuses = catalog.resolve_items(cosmetic_ids, Utc::now());

	Ok(PreviewResponse {
		bonuses: catalog.refs(&bonuses),
	})
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<PreviewRequest>,
) -> Result<Json<PreviewResponse>, PreviewError> {
	check_preview(&state.bonus_preview_limit, player.id, &body.cosmetic_ids).await?;
	Ok(Json(
		preview_outfit(&state, Viewer::Player(player.id), body.cosmetic_ids).await?,
	))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
pub(crate) async fn admin_preview(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Json(body): Json<PreviewRequest>,
) -> Result<Json<PreviewResponse>, PreviewError> {
	check_size(&body.cosmetic_ids)?;
	Ok(Json(
		preview_outfit(&state, Viewer::Admin, body.cosmetic_ids).await?,
	))
}

#[cfg(test)]
mod tests {
	use std::{collections::HashMap, time::Duration};

	use axum::{http::StatusCode, response::IntoResponse};
	use entities::sea_orm_active_enums::BodySlot;
	use sea_orm::{DbBackend, QueryTrait};

	use super::{
		MAX_PREVIEW_ITEMS, PreviewError, Viewer, check_loadout, check_preview, check_size,
		wearable_query,
	};
	use crate::api::{lockouts::LockoutLog, rate_limit::RateLimiter};

	#[tokio::test]
	async fn player_previews_are_rate_limited_even_when_oversized() {
		let limiter = RateLimiter::new("test", 2, Duration::from_secs(60), LockoutLog::default());
		assert!(check_preview(&limiter, 1, &[10, 11]).await.is_ok());
		assert!(check_preview(&limiter, 1, &[10]).await.is_ok());
		assert!(matches!(check_preview(&limiter, 1, &[10]).await, Err(PreviewError::RateLimited(_))));
		assert!(check_preview(&limiter, 2, &[10]).await.is_ok());

		let too_many = vec![1; MAX_PREVIEW_ITEMS + 1];
		assert!(matches!(check_preview(&limiter, 3, &too_many).await, Err(PreviewError::TooManyItems)));
		assert!(check_preview(&limiter, 3, &[1]).await.is_ok());
		assert!(matches!(check_preview(&limiter, 3, &[1]).await, Err(PreviewError::RateLimited(_))));
		assert!(matches!(check_size(&too_many), Err(PreviewError::TooManyItems)));
		for _ in 0..100 {
			assert!(check_size(&[1, 2]).is_ok());
		}

		let response = PreviewError::RateLimited(Duration::from_secs(12)).into_response();
		assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
		assert_eq!(response.headers()["retry-after"], "12");
		for error in [PreviewError::SlotConflict, PreviewError::NotWearable(3)] {
			assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
		}
	}

	#[test]
	fn players_preview_only_visible_or_owned_cosmetics_and_admins_anything() {
		let player = wearable_query(Viewer::Player(7), &[3, 4]).build(DbBackend::Postgres).to_string();
		let admin = wearable_query(Viewer::Admin, &[3, 4]).build(DbBackend::Postgres).to_string();
		for sql in [&player, &admin] {
			assert!(sql.contains(r#""cosmetic_allowed_slot"."cosmetic_id" IN (3, 4)"#), "{sql}");
		}
		assert!(player.contains(r#""cosmetic"."enabled" = TRUE"#), "{player}");
		assert!(player.contains(r#""player_owned_cosmetic"."player_id" = 7"#), "{player}");
		assert!(!admin.contains("enabled") && !admin.contains("player_owned_cosmetic"), "{admin}");
	}

	#[test]
	fn a_loadout_fits_one_cosmetic_per_slot() {
		use BodySlot::{Cape, EffectA, EffectB, Glasses, Hat};
		let slots: HashMap<i32, Vec<BodySlot>> = HashMap::from([
			(1, vec![Hat]),
			(2, vec![Cape]),
			(3, vec![EffectA, EffectB]),
			(4, vec![EffectA, EffectB]),
			(5, vec![EffectA, EffectB]),
			(6, vec![Hat]),
			(7, vec![Hat, Glasses]),
			(8, vec![Glasses]),
			(9, vec![]),
		]);
		for fits in [&[1, 2, 3, 4][..], &[1, 1, 2], &[7, 1], &[7, 8]] {
			assert!(check_loadout(fits, &slots).is_ok(), "{fits:?}");
		}
		for conflict in [&[1, 6][..], &[3, 4, 5], &[7, 1, 8]] {
			assert!(matches!(check_loadout(conflict, &slots), Err(PreviewError::SlotConflict)), "{conflict:?}");
		}
		assert!(matches!(check_loadout(&[1, 99], &slots), Err(PreviewError::NotWearable(99))));
		assert!(matches!(check_loadout(&[9], &slots), Err(PreviewError::NotWearable(9))));
	}
}
