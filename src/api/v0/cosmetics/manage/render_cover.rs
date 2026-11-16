use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::BodySlot;
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};

use crate::api::{ApiState, permissions::{Require, perm}};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum RenderCoverError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("The cosmetic has no source asset to render")]
	MissingAsset,
	#[error("The cosmetic already has a cover; pass force=true to regenerate")]
	CoverExists,
	#[error("Failed to render cover: {0}")]
	Render(String),
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("S3 error: {0}")]
	S3(#[from] s3::error::S3Error),
	#[error("Failed to store cover asset: {0}")]
	Store(String),
}

impl IntoResponse for RenderCoverError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::MissingAsset | Self::CoverExists => StatusCode::BAD_REQUEST,
				Self::Render(_) => StatusCode::BAD_GATEWAY,
				Self::Database(_) | Self::S3(_) | Self::Store(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RenderCoverRequest {
	cosmetic_id: i32,
	#[serde(default)]
	force: bool,
	/// Time in seconds into the emote or cosmetic animation the cover is
	/// captured at (default 1).
	cover_seconds: Option<f32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RenderCoverResponse {
	cosmetic_id: i32,
	cover_asset_id: i32,
	/// Set when the cosmetic moves.
	preview_asset_id: Option<i32>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("renderCosmeticCover")
		.summary("Render a cosmetic cover")
		.description(
			"Re-renders an existing cosmetic's cover image, plus a looping preview \
			 when it moves, and points the cosmetic at them. Skips cosmetics that \
			 already have a cover unless `force` is set. Requires \
			 `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<RenderCoverResponse>, _>(
			|res| res.description("The cover was rendered and stored"),
		)
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No cosmetic exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Missing or invalid credentials, or lacking the permission")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/render-cover",
		post_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Json(body): Json<RenderCoverRequest>,
) -> Result<Json<RenderCoverResponse>, RenderCoverError> {
	use entities::{cosmetic, prelude::*};

	let Some(cosmetic) = Cosmetic::find_by_id(body.cosmetic_id)
		.one(&state.database)
		.await?
	else {
		return Err(RenderCoverError::MissingCosmetic);
	};

	if cosmetic.cover_asset_id.is_some() && !body.force {
		return Err(RenderCoverError::CoverExists);
	}

	let Some(asset_id) = cosmetic.asset_id else {
		return Err(RenderCoverError::MissingAsset);
	};
	let Some(asset) = Asset::find_by_id(asset_id).one(&state.database).await? else {
		return Err(RenderCoverError::MissingAsset);
	};
	let Some(storage_path) = asset.storage_path.as_deref() else {
		return Err(RenderCoverError::MissingAsset);
	};
	let data = state.s3_bucket.get_object(storage_path).await?.to_vec();

	let slots = cosmetic_slots(&state.database, &cosmetic).await?;

	let renders = crate::api::v0::cosmetics::cover::render_cosmetic(
		&state.client,
		cosmetic.r#type.clone(),
		&slots,
		cosmetic.model_variant.as_deref(),
		body.cover_seconds,
		data,
	)
	.await
	.map_err(|error| RenderCoverError::Render(error.to_string()))?;

	let (cover_asset_id, preview_asset_id) = super::create::store_renders(&state, &renders)
		.await
		.map_err(|error| RenderCoverError::Store(error.to_string()))?;

	let mut active: cosmetic::ActiveModel = cosmetic.into();
	active.cover_asset_id = Set(Some(cover_asset_id));
	active.preview_asset_id = Set(preview_asset_id);
	let updated = active.update(&state.database).await?;

	Ok(Json(RenderCoverResponse {
		cosmetic_id: updated.id,
		cover_asset_id,
		preview_asset_id,
	}))
}

/// The slots a cosmetic renders in: its own, else its group's.
pub(super) async fn cosmetic_slots<C: sea_orm::ConnectionTrait>(
	db: &C,
	cosmetic: &entities::cosmetic::Model,
) -> Result<Vec<BodySlot>, sea_orm::DbErr> {
	use entities::{cosmetic_allowed_slot, cosmetic_group_allowed_slot, prelude::*};

	let slots: Vec<BodySlot> = CosmeticAllowedSlot::find()
		.filter(cosmetic_allowed_slot::Column::CosmeticId.eq(cosmetic.id))
		.all(db)
		.await?
		.into_iter()
		.map(|row| row.slot)
		.collect();
	match cosmetic.group_id {
		Some(group_id) if slots.is_empty() => Ok(CosmeticGroupAllowedSlot::find()
			.filter(cosmetic_group_allowed_slot::Column::GroupId.eq(group_id))
			.all(db)
			.await?
			.into_iter()
			.map(|row| row.slot)
			.collect()),
		_ => Ok(slots),
	}
}
