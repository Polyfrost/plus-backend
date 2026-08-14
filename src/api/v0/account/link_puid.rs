use aide::{
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode};
use sea_orm::{ActiveModelTrait, Set};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::api::{ApiState, v0::account::AuthenticatedPlayer};

#[derive(Debug, Deserialize, JsonSchema)]
struct LinkPuidRequest {
	product_user_id: String,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("linkEosProductUserId")
		.summary("Link the caller's EOS ProductUserId")
		.description(
			"Stores the caller's EOS Connect ProductUserId against their \
			 account, so friends/session invites can hand it out directly \
			 without every client needing to resolve it via EOS's own \
			 external account mapping query.",
		)
		.tag("account")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/link-puid", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<LinkPuidRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
	let mut active: entities::user::ActiveModel = player.into();
	active.eos_product_user_id = Set(Some(body.product_user_id));
	active.update(&state.database).await.map_err(|error| {
		tracing::error!(%error, "unable to link EOS product user id");
		(
			StatusCode::INTERNAL_SERVER_ERROR,
			crate::api::INTERNAL_MESSAGE.to_string()
		)
	})?;

	Ok(StatusCode::NO_CONTENT)
}
