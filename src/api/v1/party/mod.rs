mod invites;
mod live;
pub(in crate::api) mod models;
mod play;
mod roster;
mod routes;
mod voice;

use std::time::Duration;

use aide::{OperationIo, axum::ApiRouter};
use axum::{
	Json,
	http::{HeaderValue, StatusCode, header},
	response::{IntoResponse, Response},
};

pub(in crate::api) use self::{
	invites::spawn_invite_sweeper,
	live::PartyRuntime,
	play::{cancel_play_together, respond_to_play_together, start_play_together},
	roster::{
		apply_member_state, on_player_offline, on_player_online, refresh_party,
		spawn_offline_sweep,
	},
	voice::spawn_puid_verification,
};
use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub(in crate::api) enum PartyError {
	#[error("You are not in a party")]
	NotInParty,
	#[error("You are already in a party")]
	AlreadyInParty,
	#[error("Only the party leader may do that")]
	NotLeader,
	#[error("That player is not in your party")]
	MemberNotFound,
	#[error("You cannot kick yourself, leave the party instead")]
	CannotKickSelf,
	#[error("No such player")]
	PlayerNotFound,
	#[error("Only the party leader may invite players to this party")]
	NotAllowedToInvite,
	#[error("You may only invite friends to your party")]
	NotFriends,
	#[error("That player is blocked, or has blocked you")]
	Blocked,
	#[error("That player is already in your party")]
	AlreadyMember,
	#[error("That player already has a pending invite to your party")]
	AlreadyInvited,
	#[error("That party is full")]
	PartyFull,
	#[error("No such invite")]
	InviteNotFound,
	#[error("That invite has expired")]
	InviteExpired,
	#[error("The party is using peer-to-peer voice")]
	TransportP2p,
	#[error("Your EOS account is not linked, finish EOS login and link it again")]
	EosNotLinked,
	#[error("Party voice is unavailable right now")]
	VoiceUnavailable,
	#[error("No such session")]
	SessionNotFound,
	#[error("Only the session host may do that")]
	NotHost,
	#[error("Too many requests, please slow down")]
	RateLimited(Duration),
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Unable to sign the voice pass: {0}")]
	Signing(#[from] jsonwebtoken::errors::Error),
	#[error("Unable to send session invites: {0}")]
	Session(crate::api::v0::sessions::SessionError),
}

impl PartyError {
	fn status(&self) -> StatusCode {
		match self {
			Self::NotInParty
			| Self::MemberNotFound
			| Self::PlayerNotFound
			| Self::InviteNotFound
			| Self::SessionNotFound => StatusCode::NOT_FOUND,
			Self::NotLeader | Self::NotAllowedToInvite | Self::NotHost => {
				StatusCode::FORBIDDEN
			}
			Self::AlreadyInParty
			| Self::CannotKickSelf
			| Self::NotFriends
			| Self::Blocked
			| Self::AlreadyMember
			| Self::AlreadyInvited
			| Self::PartyFull
			| Self::TransportP2p
			| Self::EosNotLinked => StatusCode::CONFLICT,
			Self::InviteExpired => StatusCode::GONE,
			Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
			Self::VoiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
			Self::Database(_) | Self::Signing(_) | Self::Session(_) => {
				StatusCode::INTERNAL_SERVER_ERROR
			}
		}
	}

	fn code(&self) -> &'static str {
		match self {
			Self::NotInParty => "not_in_party",
			Self::AlreadyInParty => "already_in_party",
			Self::NotLeader => "not_leader",
			Self::MemberNotFound => "member_not_found",
			Self::CannotKickSelf => "cannot_kick_self",
			Self::PlayerNotFound => "player_not_found",
			Self::NotAllowedToInvite => "not_allowed_to_invite",
			Self::NotFriends => "not_friends",
			Self::Blocked => "blocked",
			Self::AlreadyMember => "already_member",
			Self::AlreadyInvited => "already_invited",
			Self::PartyFull => "party_full",
			Self::InviteNotFound => "invite_not_found",
			Self::InviteExpired => "invite_expired",
			Self::TransportP2p => "transport_p2p",
			Self::EosNotLinked => "eos_not_linked",
			Self::VoiceUnavailable => "voice_unavailable",
			Self::SessionNotFound => "session_not_found",
			Self::NotHost => "not_host",
			Self::RateLimited(_) => "rate_limited",
			Self::Database(_) | Self::Signing(_) | Self::Session(_) => "internal",
		}
	}
}

impl IntoResponse for PartyError {
	fn into_response(self) -> Response {
		let status = self.status();
		let message =
			if status.is_server_error() && !matches!(self, Self::VoiceUnavailable) {
				tracing::error!(%status, error = %self, "party request failed");
				crate::api::INTERNAL_MESSAGE.to_string()
			} else {
				self.to_string()
			};

		let body = serde_json::json!({ "error": self.code(), "message": message });
		let mut response = (status, Json(body)).into_response();
		if let Self::RateLimited(wait) = self
			&& let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string())
		{
			response.headers_mut().insert(header::RETRY_AFTER, value);
		}
		response
	}
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(routes::router())
		.merge(invites::router())
		.merge(voice::router())
		.merge(play::router())
}
