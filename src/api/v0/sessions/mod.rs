mod host;
mod invites;

use aide::{OperationIo, axum::ApiRouter};
use axum::{http::StatusCode, response::IntoResponse};
use chrono::Duration;
use entities::sea_orm_active_enums::SessionInviteStatus;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use tracing::{info, warn};

use crate::api::ApiState;

pub(super) const SESSION_TTL_HOURS: i64 = 12;

const DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) fn session_ttl() -> Duration {
	Duration::hours(SESSION_TTL_HOURS)
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
	#[error(transparent)]
	Group(#[from] crate::api::v0::groups::GroupError),
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for SessionError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				// Defer to the group error so its own status mapping is kept
				Self::Group(error) => return error.into_response(),
				Self::PlayerMissing | Self::SessionMissing | Self::InviteMissing => {
					StatusCode::NOT_FOUND
				}
				Self::SelfTarget | Self::Blocked | Self::NotFriends | Self::SessionExpired => {
					StatusCode::CONFLICT
				}
				Self::NotHost | Self::InviteForbidden => StatusCode::FORBIDDEN,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
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

async fn expire_sessions_of_host(
	state: &ApiState,
	host: uuid::Uuid,
) -> Result<(), SessionError> {
	use entities::{game_sessions, prelude::*};
	use sea_orm::{prelude::DateTimeWithTimeZone, sea_query::Expr};

	let host = find_user_by_uuid(state, host).await?;
	let now = chrono::Utc::now();

	let sessions = GameSessions::find()
		.filter(game_sessions::Column::HostId.eq(host.id))
		.filter(game_sessions::Column::ExpiresAt.gt(now))
		.all(&state.database)
		.await?;
	if sessions.is_empty() {
		return Ok(());
	}

	let ids = sessions
		.iter()
		.map(|session| session.id)
		.collect::<Vec<_>>();

	GameSessions::update_many()
		.col_expr(
			game_sessions::Column::ExpiresAt,
			Expr::value(DateTimeWithTimeZone::from(now)),
		)
		.filter(game_sessions::Column::Id.is_in(ids.clone()))
		.exec(&state.database)
		.await?;

	let expired = invites::expire_invites_for_sessions(state, ids).await?;
	info!(
		host = %host.minecraft_uuid,
		sessions = sessions.len(),
		invites = expired,
		"Expired hosted sessions after their host disconnected"
	);

	Ok(())
}

pub(in crate::api) fn expire_sessions_after_grace(state: &ApiState, host: uuid::Uuid) {
	let state = state.clone();

	tokio::spawn(async move {
		tokio::time::sleep(DISCONNECT_GRACE).await;

		if state
			.realtime
			.connections_by_owner
			.read()
			.await
			.contains_key(&host)
		{
			return;
		}

		if let Err(error) = expire_sessions_of_host(&state, host).await {
			warn!("Unable to expire hosted sessions for {host}: {error}");
		}
	});
}

pub(in crate::api) fn spawn_session_sweeper(state: ApiState) {
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(SWEEP_INTERVAL);
		loop {
			interval.tick().await;

			if let Err(error) = sweep_expired_invites(&state).await {
				warn!("Unable to sweep expired session invites: {error}");
			}
		}
	});
}

async fn sweep_expired_invites(state: &ApiState) -> Result<(), SessionError> {
	use entities::{game_sessions, prelude::*, session_invites};

	let pending = SessionInvites::find()
		.filter(session_invites::Column::Status.eq(SessionInviteStatus::Pending))
		.all(&state.database)
		.await?;
	if pending.is_empty() {
		return Ok(());
	}

	let mut session_ids = pending
		.iter()
		.map(|invite| invite.session_id)
		.collect::<Vec<_>>();
	session_ids.sort_unstable();
	session_ids.dedup();

	let expired = GameSessions::find()
		.filter(game_sessions::Column::Id.is_in(session_ids))
		.filter(game_sessions::Column::ExpiresAt.lte(chrono::Utc::now()))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|session| session.id)
		.collect::<Vec<_>>();

	let count = invites::expire_invites_for_sessions(state, expired).await?;
	if count > 0 {
		info!(invites = count, "Expired invites whose session is dead");
	}

	Ok(())
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/sessions",
		ApiRouter::new().merge(host::router()).merge(invites::router()),
	)
}
