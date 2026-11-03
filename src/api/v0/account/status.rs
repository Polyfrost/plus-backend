use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::get_with,
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, NoContent},
};
use entities::{prelude::*, sea_orm_active_enums::PlayerStatus, user};
use schemars::JsonSchema;
use sea_orm::{ActiveValue::Set, EntityTrait as _};
use serde::{Deserialize, Serialize};

use crate::api::{
	ApiState, state::effective_status, v0::account::AuthenticatedPlayer,
	v0::websocket::announce_presence,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum StatusError {
	#[error("You are changing your status too quickly, please slow down")]
	RateLimited,
	#[error("Unable to update status: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for StatusError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RequestBody {
	status: PlayerStatus,
}

#[derive(Debug, Serialize, JsonSchema)]
struct StatusResponse {
	/// The status the player picked; `online` also means no override.
	status: PlayerStatus,
}

fn view_doc(op: TransformOperation) -> TransformOperation {
	op.id("getPlayerStatus")
		.summary("Get the player's status")
		.description("Returns the status the authorized player picked, `online` when they picked none.")
		.tag("account")
}

#[tracing::instrument(level = "debug")]
async fn view(
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Json<StatusResponse> {
	// AuthenticatedPlayer loads the row per request, so this is current.
	Json(StatusResponse { status: player.status_override.unwrap_or(PlayerStatus::Online) })
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("putPlayerStatus")
		.summary("Set the player's status")
		.description(
			"Sets the status other players see while the authorized player is connected. \
			 `online` clears any override, so the status follows the connection again. \
			 A disconnected player always shows as offline. This only hides presence, \
			 including online and live state in the party roster: \
			 cosmetic and emote updates still reach players who can see them in-game.",
		)
		.tag("account")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/status",
		get_with(self::view, self::view_doc).put_with(self::endpoint, self::endpoint_doc),
	)
}

/// Picking online is the same as having no override.
fn to_override(status: PlayerStatus) -> Option<PlayerStatus> {
	(status != PlayerStatus::Online).then_some(status)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<RequestBody>,
) -> Result<NoContent, StatusError> {
	// Each change is broadcast to every watcher, so keep toggling from flooding them.
	if state.status_limit.check(player.id).await.is_some() {
		return Err(StatusError::RateLimited);
	}

	let status_override = to_override(body.status);

	User::update(user::ActiveModel {
		id: Set(player.id),
		status_override: Set(status_override),
		..Default::default()
	})
	.exec(&state.database)
	.await?;

	let owner = player.minecraft_uuid;
	let connected = state.realtime.connections_by_owner.read().await.contains_key(&owner);
	let changed = state
		.realtime
		.player_runtime
		.write()
		.await
		.get_mut(&owner)
		.and_then(|runtime| {
			let before = effective_status(connected, runtime.status_override);
			runtime.status_override = status_override;
			let after = effective_status(connected, status_override);
			(before != after).then_some(after)
		});

	if let Some(status) = changed {
		announce_presence(&state, owner, status).await;
		crate::api::v1::party::refresh_party(&state, owner).await;
	}

	Ok(NoContent)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn online_clears_override() {
		assert_eq!(to_override(PlayerStatus::Online), None);
		assert_eq!(
			to_override(PlayerStatus::Offline),
			Some(PlayerStatus::Offline)
		);
	}

	#[test]
	fn parses_request_body() {
		let body: RequestBody = serde_json::from_str(r#"{"status":"offline"}"#)
			.expect("body should parse");
		assert_eq!(body.status, PlayerStatus::Offline);
	}
}
