use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, Query, State},
	http::StatusCode,
	response::IntoResponse,
};
use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ViewError {
	#[error("No enabled cosmetic with that id exists")]
	NotFound,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for ViewError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ViewQuery {
	/// Restrict the lookup to a single cosmetic type (including `emote`). Omit to
	/// match any type.
	r#type: Option<CosmeticType>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ViewResponse {
	/// The Stripe price id for this cosmetic, if one is set.
	stripe_price_id: Option<String>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("viewCosmetic")
		.summary("View a cosmetic")
		.description(
			"Returns the Stripe price id of an enabled cosmetic (including emotes). \
			 Use `type` to restrict the lookup to a single cosmetic type.",
		)
		.tag("cosmetics")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/cosmetics/view/{id}",
		get_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	Path(id): Path<i32>,
	Query(query): Query<ViewQuery>,
) -> Result<Json<ViewResponse>, ViewError> {
	use entities::{cosmetic, prelude::*};

	// Emotes are cosmetics with type `emote`, so a single cosmetic lookup covers
	// every type. A `type` filter (including `emote`) narrows the lookup. Only
	// enabled rows are viewable, mirroring the search endpoint.
	let mut find = Cosmetic::find_by_id(id).filter(cosmetic::Column::Enabled.eq(true));
	if let Some(kind) = &query.r#type {
		find = find.filter(cosmetic::Column::Type.eq(kind.clone()));
	}

	let cosmetic = find.one(&state.database).await?.ok_or(ViewError::NotFound)?;

	Ok(Json(ViewResponse {
		stripe_price_id: cosmetic.stripe_price_id,
	}))
}
