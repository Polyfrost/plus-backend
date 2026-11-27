use std::collections::{HashMap, HashSet};

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Query, State},
	http::StatusCode,
	response::IntoResponse,
};
use entities::{
	cosmetic, cosmetic_group, player_equipped_cosmetic, player_owned_cosmetic,
	prelude::*, sea_orm_active_enums::BodySlot, user,
};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	state::PlayerRuntimeState,
	v0::{
		account::OptionalAuthenticationExtractor,
		cosmetics::{CosmeticInfo, group_cosmetics, load_assets, load_groups},
	},
};

const MAX_IDS: usize = 100;

type Groups = HashMap<i32, (cosmetic_group::Model, Vec<BodySlot>)>;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("No cosmetic ids were given")]
	NoIds,
	#[error("At most {} cosmetic ids can be looked up at once", MAX_IDS)]
	TooManyIds,
	#[error("The ids must be a comma-separated list of cosmetic ids")]
	Malformed,
	#[error("Unable to fetch cosmetic data from database: {0}")]
	DatabaseFetch(#[from] DbErr),
	#[error("Unable to read assets from object storage: {0}")]
	S3(#[from] s3::error::S3Error),
}

impl IntoResponse for ResponseError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::NoIds | Self::TooManyIds | Self::Malformed => {
					StatusCode::BAD_REQUEST
				}
				Self::DatabaseFetch(_) | Self::S3(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("getCosmeticDefinitions")
		.summary("Look up cosmetics by id")
		.description(
			"Returns the definitions of the given cosmetic variant ids in the same \
			 shape as the cosmetic list, including variants the list leaves out. \
			 A hidden cosmetic is only returned while someone has it equipped, or \
			 when the request is authenticated as a player who owns it. Unknown \
			 ids are left out.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"No ids were given, more than 100 were given, or the list is malformed",
			)
		})
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to fetch cosmetics",
				)
			},
		)
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryParams {
	#[serde(default)]
	#[schemars(
		description = "Comma-separated cosmetic variant ids, at most 100",
		example = &"12,40,41"
	)]
	ids: String,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Response {
	cosmetics: Vec<CosmeticInfo>,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/definitions", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	OptionalAuthenticationExtractor(authenticated): OptionalAuthenticationExtractor,
	Query(query): Query<QueryParams>,
) -> Result<Json<Response>, ResponseError> {
	let ids = parse_ids(&query.ids)?;

	let found = Cosmetic::find()
		.filter(cosmetic::Column::Id.is_in(ids))
		.find_with_related(CosmeticAllowedSlot)
		.all(&state.database)
		.await?;
	let groups = load_groups(&state.database).await?;

	let hidden = hidden_ids(&found, &groups);
	let shared = if hidden.is_empty() {
		HashSet::new()
	} else {
		shared_hidden(&state, authenticated, &hidden).await?
	};

	let found = visible_rows(found, &groups, &shared);
	if found.is_empty() {
		return Ok(Json(Response::default()));
	}

	let assets = load_assets(&state.database, found.iter().map(|(c, _)| c)).await?;
	let rows = found
		.into_iter()
		.map(|(cosmetic, allowed)| {
			let asset = cosmetic.asset_id.and_then(|id| assets.get(&id).cloned());
			let cover_asset = cosmetic
				.cover_asset_id
				.and_then(|id| assets.get(&id).cloned());
			let allowed_slots = allowed.into_iter().map(|s| s.slot).collect();
			(cosmetic, asset, cover_asset, allowed_slots)
		})
		.collect();

	let cosmetics = group_cosmetics(
		rows,
		groups,
		state.asset_cache.clone(),
		state.s3_bucket.clone(),
		&state.s3_public_url,
		false,
	)
	.await?;

	Ok(Json(Response { cosmetics }))
}

fn parse_ids(raw: &str) -> Result<Vec<i32>, ResponseError> {
	let raw = raw.trim();
	if raw.is_empty() {
		return Err(ResponseError::NoIds);
	}

	let parts: Vec<&str> = raw.split(',').collect();
	if parts.len() > MAX_IDS {
		return Err(ResponseError::TooManyIds);
	}

	let mut ids = Vec::with_capacity(parts.len());
	for part in parts {
		let id = part
			.trim()
			.parse::<i32>()
			.map_err(|_| ResponseError::Malformed)?;
		if !ids.contains(&id) {
			ids.push(id);
		}
	}
	Ok(ids)
}

fn publicly_listed(cosmetic: &cosmetic::Model, groups: &Groups) -> bool {
	cosmetic.enabled
		&& cosmetic
			.group_id
			.is_none_or(|id| groups.get(&id).is_some_and(|(group, _)| group.enabled))
}

fn hidden_ids<T>(found: &[(cosmetic::Model, T)], groups: &Groups) -> Vec<i32> {
	found
		.iter()
		.map(|(cosmetic, _)| cosmetic)
		.filter(|cosmetic| !publicly_listed(cosmetic, groups))
		.map(|cosmetic| cosmetic.id)
		.collect()
}

fn visible_rows<T>(
	found: Vec<(cosmetic::Model, T)>,
	groups: &Groups,
	shared: &HashSet<i32>,
) -> Vec<(cosmetic::Model, T)> {
	found
		.into_iter()
		.filter(|(cosmetic, _)| {
			publicly_listed(cosmetic, groups) || shared.contains(&cosmetic.id)
		})
		.collect()
}

fn worn_live(
	runtime: &HashMap<Uuid, PlayerRuntimeState>,
	hidden: &[i32],
) -> HashSet<i32> {
	runtime
		.values()
		.flat_map(|player| player.equipped.values())
		.filter(|id| hidden.contains(id))
		.copied()
		.collect()
}

async fn shared_hidden(
	state: &ApiState,
	authenticated: Option<Uuid>,
	hidden: &[i32],
) -> Result<HashSet<i32>, DbErr> {
	let mut shared = worn_live(&*state.realtime.player_runtime.read().await, hidden);

	for &id in hidden {
		if shared.contains(&id) {
			continue;
		}
		let worn = PlayerEquippedCosmetic::find()
			.select_only()
			.column(player_equipped_cosmetic::Column::CosmeticId)
			.filter(player_equipped_cosmetic::Column::CosmeticId.eq(id))
			.limit(1)
			.into_tuple::<i32>()
			.one(&state.database)
			.await?;
		shared.extend(worn);
	}

	if let Some(uuid) = authenticated {
		shared.extend(
			PlayerOwnedCosmetic::find()
				.select_only()
				.column(player_owned_cosmetic::Column::CosmeticId)
				.inner_join(User)
				.filter(user::Column::MinecraftUuid.eq(uuid))
				.filter(player_owned_cosmetic::Column::CosmeticId.is_in(hidden.to_vec()))
				.into_tuple::<i32>()
				.all(&state.database)
				.await?,
		);
	}

	Ok(shared)
}

#[cfg(test)]
mod tests {
	use std::collections::{HashMap, HashSet};

	use axum::{http::StatusCode, response::IntoResponse};
	use entities::{
		cosmetic, cosmetic_group,
		sea_orm_active_enums::{BodySlot, CosmeticType},
	};
	use uuid::Uuid;

	use super::{
		Groups, MAX_IDS, Response, ResponseError, hidden_ids, parse_ids, visible_rows, worn_live,
	};
	use crate::api::state::PlayerRuntimeState;

	fn cosmetic(id: i32, enabled: bool, group_id: Option<i32>) -> (cosmetic::Model, ()) {
		(cosmetic::Model { enabled, group_id, ..super::super::tests::cosmetic(id, CosmeticType::Cape) }, ())
	}

	fn groups() -> Groups {
		let group = |id, enabled| {
			let name = format!("Group {id}");
			let (created_at, updated_at) = (Default::default(), Default::default());
			let model = cosmetic_group::Model { id, r#type: CosmeticType::Cape, name, enabled, created_at, updated_at };
			(id, (model, vec![BodySlot::Cape]))
		};
		HashMap::from([group(1, true), group(2, false)])
	}

	fn ids(rows: &[(cosmetic::Model, ())]) -> Vec<i32> {
		rows.iter().map(|(c, _)| c.id).collect()
	}

	#[test]
	fn id_lists_are_bounded_and_well_formed() {
		assert_eq!(parse_ids("1, 2,3,2").ok(), Some(vec![1, 2, 3]));
		let list = |count: i32| (1..=count).map(|id| id.to_string()).collect::<Vec<_>>().join(",");
		assert_eq!(parse_ids(&list(MAX_IDS as i32)).map(|ids| ids.len()).ok(), Some(MAX_IDS));
		assert!(matches!(parse_ids(&list(MAX_IDS as i32 + 1)), Err(ResponseError::TooManyIds)));
		for raw in ["1,,2", "1,a", "1;2", "1,", "99999999999"] {
			assert!(matches!(parse_ids(raw), Err(ResponseError::Malformed)), "{raw}");
		}
		for raw in ["", "  "] {
			assert!(matches!(parse_ids(raw), Err(ResponseError::NoIds)));
		}
		for error in [ResponseError::TooManyIds, ResponseError::Malformed, ResponseError::NoIds] {
			assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
		}
		assert_eq!(serde_json::to_value(Response::default()).unwrap(), serde_json::json!({ "cosmetics": [] }));
	}

	#[test]
	fn hidden_cosmetics_are_returned_only_while_someone_wears_them() {
		let groups = groups();
		let wearing = |ids: &[i32]| {
			let equipped = ids.iter().zip([BodySlot::Cape, BodySlot::Hat]).map(|(id, slot)| (slot, *id)).collect();
			HashMap::from([(Uuid::new_v4(), PlayerRuntimeState { equipped, ..Default::default() })])
		};
		let found = || vec![cosmetic(10, true, None), cosmetic(11, true, Some(1)), cosmetic(20, false, None), cosmetic(21, true, Some(2))];

		let hidden = hidden_ids(&found(), &groups);
		assert_eq!(hidden, [20, 21]);
		assert!(hidden_ids(&found()[..2], &groups).is_empty());
		assert_eq!(ids(&visible_rows(found(), &groups, &HashSet::new())), [10, 11]);
		let shared = worn_live(&wearing(&[20, 21]), &hidden);
		assert_eq!(ids(&visible_rows(found(), &groups, &shared)), [10, 11, 20, 21]);
		let shared = worn_live(&wearing(&[10, 99]), &hidden);
		assert!(shared.is_empty());
		assert_eq!(ids(&visible_rows(found(), &groups, &HashSet::from([404, 405]))), [10, 11]);
	}
}
