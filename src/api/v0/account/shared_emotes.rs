use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, NoContent},
};
use entities::{prelude::*, sea_orm_active_enums::SharedEmotePrivacy, user};
use schemars::JsonSchema;
use sea_orm::{ActiveValue::Set, EntityTrait as _};
use serde::{Deserialize, Serialize};

use crate::api::{ApiState, v0::account::AuthenticatedPlayer};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SharedEmotePrivacyError {
	#[error("Unable to update shared emote privacy: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for SharedEmotePrivacyError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(StatusCode::INTERNAL_SERVER_ERROR, self)
	}
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SharedEmotePrivacyBody {
	privacy: SharedEmotePrivacy,
}

fn view_doc(op: TransformOperation) -> TransformOperation {
	op.id("getSharedEmotePrivacy")
		.summary("Get who can send you shared emote requests")
		.description(
			"Returns who may ask the authorized player to play a shared emote: \
			 `everyone`, `friends` or `nobody`.",
		)
		.tag("account")
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("putSharedEmotePrivacy")
		.summary("Set who can send you shared emote requests")
		.description(
			"Sets who may ask the authorized player to play a shared emote. Requests from \
			 anyone else, and from blocked players, are declined without reaching them.",
		)
		.tag("account")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/shared-emote-privacy",
		get_with(self::view, self::view_doc).put_with(self::update, self::update_doc),
	)
}

#[tracing::instrument(level = "debug")]
async fn view(
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Json<SharedEmotePrivacyBody> {
	Json(SharedEmotePrivacyBody {
		privacy: player.shared_emote_privacy,
	})
}

#[tracing::instrument(level = "debug", skip(state))]
async fn update(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<SharedEmotePrivacyBody>,
) -> Result<NoContent, SharedEmotePrivacyError> {
	User::update(user::ActiveModel {
		id: Set(player.id),
		shared_emote_privacy: Set(body.privacy),
		..Default::default()
	})
	.exec(&state.database)
	.await?;

	Ok(NoContent)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_every_privacy_choice() {
		for (raw, expected) in [
			("everyone", SharedEmotePrivacy::Everyone),
			("friends", SharedEmotePrivacy::Friends),
			("nobody", SharedEmotePrivacy::Nobody),
		] {
			let body: SharedEmotePrivacyBody =
				serde_json::from_str(&format!(r#"{{"privacy":"{raw}"}}"#))
					.expect("body should parse");
			assert_eq!(body.privacy, expected);
		}
	}
}
