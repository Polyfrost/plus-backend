use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset};
use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Serialize;

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

#[derive(Debug, Serialize, JsonSchema)]
pub struct ViewResponse {
	/// The Stripe price id for this cosmetic, if one is set.
	stripe_price_id: Option<String>,
	id: i32,
	name: String,
	description: Option<String>,
	collection: Option<i32>,
	r#type: CosmeticType,
	base_price: Option<f32>,
	discount_rate: Option<i32>,
	asset_id: Option<i32>,
	created_at: DateTime<FixedOffset>,
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
) -> Result<Json<ViewResponse>, ViewError> {
	use entities::{cosmetic, prelude::*};

	if let Some(cosmetic) = Cosmetic::find_by_id(id)
		.filter(cosmetic::Column::Enabled.eq(true))
		.one(&state.database)
		.await?
	{
		Ok(Json(ViewResponse {
			stripe_price_id: cosmetic.stripe_price_id,
			id: cosmetic.id,
			name: cosmetic
				.name
				.unwrap_or_else(|| format!("Cosmetic {}", cosmetic.id)),
			description: cosmetic.description,
			collection: cosmetic.collection,
			r#type: cosmetic.r#type,
			base_price: cosmetic.base_price,
			discount_rate: cosmetic.discount_rate,
			asset_id: cosmetic.asset_id,
			created_at: cosmetic.created_at,
		}))
	} else {
		Err(ViewError::NotFound)
	}
}
