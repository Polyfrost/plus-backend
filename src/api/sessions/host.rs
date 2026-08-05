use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use chrono::{DateTime, FixedOffset, Utc};
use schemars::JsonSchema;
use sea_orm::{ActiveValue, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{ApiState, account::AuthenticatedPlayer, sessions::SessionError};

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSessionRequest {
	eos_session_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionResponse {
	id: Uuid,
	eos_session_id: Option<String>,
	expires_at: DateTime<FixedOffset>,
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createGameSession")
		.summary("Host a multiplayer session")
		.description(
			"Registers a session the caller is hosting so friend invites can \
			 be sent for it. The actual P2P transport is established \
			 client-side over EOS; this only tracks invite lifecycle.",
		)
		.tag("sessions")
}

fn close_doc(op: TransformOperation) -> TransformOperation {
	op.id("closeGameSession")
		.summary("Close a hosted session early")
		.tag("sessions")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/", post_with(self::create, self::create_doc))
		.api_route("/{id}", delete_with(self::close, self::close_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn create(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionResponse>), SessionError> {
	use entities::{game_sessions, prelude::*};

	let expires_at = Utc::now() + super::session_ttl();

	let session = GameSessions::insert(game_sessions::ActiveModel {
		id: Set(Uuid::now_v7()),
		host_id: Set(player.id),
		eos_session_id: Set(body.eos_session_id),
		created_at: ActiveValue::NotSet,
		expires_at: Set(expires_at.into()),
	})
	.exec_with_returning(&state.database)
	.await?;

	Ok((
		StatusCode::CREATED,
		Json(SessionResponse {
			id: session.id,
			eos_session_id: session.eos_session_id,
			expires_at: session.expires_at,
		}),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn close(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<Uuid>,
) -> Result<StatusCode, SessionError> {
	use entities::prelude::*;

	let session = super::load_session(&state, id).await?;
	if session.host_id != player.id {
		return Err(SessionError::NotHost);
	}

	GameSessions::delete_by_id(id).exec(&state.database).await?;

	Ok(StatusCode::NO_CONTENT)
}
