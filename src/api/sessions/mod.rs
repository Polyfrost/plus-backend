mod host;
mod invites;

use aide::{OperationIo, axum::ApiRouter};
use axum::{http::StatusCode, response::IntoResponse};
use chrono::Duration;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::api::ApiState;

pub(super) const SESSION_TTL_MINUTES: i64 = 15;

pub(super) fn session_ttl() -> Duration {
	Duration::minutes(SESSION_TTL_MINUTES)
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SessionError {
	#[error("The requested player does not exist")]
	PlayerMissing,
	#[error("You cannot perform this action on yourself")]
	SelfTarget,
	#[error("That player is blocked, or has blocked you")]
	Blocked,
	#[error(
		"Multiplayer session invites may only be sent to friends, per the \
		 Friends System"
	)]
	NotFriends,
	#[error("No such session")]
	SessionMissing,
	#[error("This session has expired")]
	SessionExpired,
	#[error("Only the session host may perform this action")]
	NotHost,
	#[error("No such invite")]
	InviteMissing,
	#[error("You do not have permission to act on that invite")]
	InviteForbidden,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for SessionError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::PlayerMissing | Self::SessionMissing | Self::InviteMissing => {
					StatusCode::NOT_FOUND
				}
				Self::SelfTarget | Self::Blocked | Self::NotFriends | Self::SessionExpired => {
					StatusCode::CONFLICT
				}
				Self::NotHost | Self::InviteForbidden => StatusCode::FORBIDDEN,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

pub(super) async fn find_user_by_uuid(
	state: &ApiState,
	uuid: uuid::Uuid,
) -> Result<entities::user::Model, SessionError> {
	use entities::{prelude::*, user};

	User::find()
		.filter(user::Column::MinecraftUuid.eq(uuid))
		.one(&state.database)
		.await?
		.ok_or(SessionError::PlayerMissing)
}

pub(super) async fn load_session(
	state: &ApiState,
	id: uuid::Uuid,
) -> Result<entities::game_sessions::Model, SessionError> {
	use entities::prelude::*;

	let session = GameSessions::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(SessionError::SessionMissing)?;

	if session.expires_at < chrono::Utc::now() {
		return Err(SessionError::SessionExpired);
	}

	Ok(session)
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/sessions",
		ApiRouter::new().merge(host::router()).merge(invites::router()),
	)
}
