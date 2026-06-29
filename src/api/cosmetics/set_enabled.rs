use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde::Deserialize;

use crate::api::{ApiState, admin_auth::AdminAuthenticationExtractor};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SetEnabledError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for SetEnabledError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("setCosmeticEnabled")
		.summary("Enable or disable a cosmetic")
		.description(
			"Toggles a single cosmetic variant's `enabled` flag. Disabled \
			 cosmetics are hidden from the public catalog and from players' owned \
			 lists, but their rows and assets are left intact so the change is \
			 reversible. Admin password required.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The cosmetic's enabled flag was updated")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No cosmetic exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetEnabledRequest {
	cosmetic_id: i32,
	enabled: bool,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/set_enabled", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<SetEnabledRequest>,
) -> Result<StatusCode, SetEnabledError> {
	use entities::{cosmetic, prelude::*};

	let Some(model) = Cosmetic::find_by_id(body.cosmetic_id)
		.one(&state.database)
		.await?
	else {
		return Err(SetEnabledError::MissingCosmetic);
	};

	if model.enabled != body.enabled {
		let mut active: cosmetic::ActiveModel = model.into();
		active.enabled = Set(body.enabled);
		active.update(&state.database).await?;
	}

	Ok(StatusCode::NO_CONTENT)
}
