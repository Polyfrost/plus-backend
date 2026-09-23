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
use entities::{cosmetic_allowed_slot, sea_orm_active_enums::BodySlot};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait as _, EntityTrait, QueryFilter, QueryTrait as _};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::{
		account::OptionalAuthenticationExtractor,
		cosmetics::{
			CosmeticInfo, EmoteInfo, EquippedCosmetics, group_cosmetics, load_assets,
			load_groups,
		},
	},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("Authentication was not given, so a player query parameter is required")]
	PlayerRequired,
	#[error("Unable to fetch user data from database: {0}")]
	DatabaseFetch(#[from] sea_orm::error::DbErr),
	#[error("Unable to read assets from object storage: {0}")]
	S3(#[from] s3::error::S3Error),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("getPlayerCosmetics")
		.summary("Get a player's cosmetic status")
		.description(
			"Lists a player's equipped cosmetics. Every owned cosmetic is included \
			 only when the request is authenticated as that player; for anyone else \
			 the listing is narrowed to what they currently wear.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"Authentication was not given, so a player query parameter is required",
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

impl IntoResponse for ResponseError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				ResponseError::PlayerRequired => StatusCode::BAD_REQUEST,
				ResponseError::S3(_) => StatusCode::INTERNAL_SERVER_ERROR,
				ResponseError::DatabaseFetch(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryParams {
	/// The UUID of the player to look up the cosmetics of. This is only
	/// optional if authentication is passed instead.
	#[serde(default)]
	#[schemars(example = &"f7c77d99-9f15-4a66-a87d-c4a51ef30d19")]
	player: Option<Uuid>,
}

/// Information about the player's cosmetics
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Response {
	cosmetics: Vec<CosmeticInfo>,
	emotes: Vec<EmoteInfo>,
	equipped: EquippedCosmetics,
	particle_color: Option<i32>,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/player", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	OptionalAuthenticationExtractor(authenticated): OptionalAuthenticationExtractor,
	Query(query): Query<QueryParams>,
) -> Result<Json<Response>, ResponseError> {
	let mut response = Response::default();
	let Some(uuid) = query.player.or(authenticated) else {
		return Err(ResponseError::PlayerRequired);
	};
	let is_self = Some(uuid) == authenticated;

	{
		use std::collections::HashMap;

		use entities::{
			player_equipped_cosmetic, player_owned_cosmetic, prelude::*,
			sea_orm_active_enums::CosmeticType, user,
		};

		let Some(target) = User::find()
			.filter(user::Column::MinecraftUuid.eq(uuid))
			.one(&state.database)
			.await?
		else {
			return Ok(Json(response));
		};

		response.particle_color = target.particle_color;

		response.equipped.extend(
			PlayerEquippedCosmetic::find()
				.filter(player_equipped_cosmetic::Column::PlayerId.eq(target.id))
				.find_also_related(Cosmetic)
				.all(&state.database)
				.await?
				.into_iter()
				.filter_map(|(equipment, cosmetic)| {
					cosmetic.map(|_| (equipment.slot, equipment.cosmetic_id))
				}),
		);

		let owned = PlayerOwnedCosmetic::find()
			.filter(player_owned_cosmetic::Column::PlayerId.eq(target.id))
			.apply_if(
				(!is_self)
					.then(|| response.equipped.values().copied().collect::<Vec<_>>()),
				|query, equipped| {
					query
						.filter(player_owned_cosmetic::Column::CosmeticId.is_in(equipped))
				},
			)
			.find_also_related(Cosmetic)
			.all(&state.database)
			.await?;

		let cosmetics: Vec<_> = owned.into_iter().filter_map(|(_, c)| c).collect();
		if cosmetics.is_empty() {
			return Ok(Json(response));
		}
		let assets = load_assets(&state.database, &cosmetics).await?;

		let mut slots: HashMap<i32, Vec<BodySlot>> = HashMap::new();
		for slot in CosmeticAllowedSlot::find()
			.filter(
				cosmetic_allowed_slot::Column::CosmeticId
					.is_in(cosmetics.iter().map(|c| c.id).collect::<Vec<_>>()),
			)
			.all(&state.database)
			.await?
		{
			slots.entry(slot.cosmetic_id).or_default().push(slot.slot);
		}

		let mut rows = Vec::with_capacity(cosmetics.len());
		let mut emote_tasks = JoinSet::new();
		for cosmetic in cosmetics {
			let asset = cosmetic.asset_id.and_then(|id| assets.get(&id).cloned());

			if matches!(cosmetic.r#type, CosmeticType::Emote) {
				let asset_cache = state.asset_cache.clone();
				let s3_bucket = state.s3_bucket.clone();
				let public_url = state.s3_public_url.clone();
				emote_tasks.spawn(async move {
					EmoteInfo::from_db_model(
						&cosmetic,
						asset.as_ref(),
						asset_cache,
						s3_bucket,
						&public_url,
					)
					.await
				});
				continue;
			}

			let cover_asset = cosmetic
				.cover_asset_id
				.and_then(|id| assets.get(&id).cloned());
			let allowed_slots = slots.remove(&cosmetic.id).unwrap_or_default();
			rows.push((cosmetic, asset, cover_asset, allowed_slots));
		}

		let groups = load_groups(&state.database).await?;
		response.cosmetics = group_cosmetics(
			rows,
			groups,
			state.asset_cache.clone(),
			state.s3_bucket.clone(),
			&state.s3_public_url,
			false,
		)
		.await?;

		response.emotes.extend(
			emote_tasks
				.join_all()
				.await
				.into_iter()
				.collect::<Result<Vec<_>, _>>()?,
		);
	};

	Ok(Json(response))
}
