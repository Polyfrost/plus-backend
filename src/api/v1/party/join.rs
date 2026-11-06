use std::{
	collections::HashMap,
	sync::Arc,
	time::{Duration, Instant},
};

use aide::{
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use chrono::{DateTime, Utc};
use entities::{
	prelude::*, sea_orm_active_enums::SessionInviteStatus, session_invites, user,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::{
	PartyError,
	models::{HostedJoinRequest, HostedJoinResult},
	play::session_error,
	roster::{connected, current_party, require_party},
};
use crate::api::{
	ApiState,
	v0::{
		account::AuthenticatedPlayer,
		sessions::{invite_to_session, load_session},
		social::{are_friends, is_blocked_either_way},
		websocket::{send_to_owner, structs::ClientBoundPacket},
	},
};

const LIFETIME: Duration = Duration::from_secs(60);
const DENY_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
pub(in crate::api) struct JoinRequest {
	id: Uuid,
	session_id: Uuid,
	party_id: Uuid,
	host_id: i32,
	host: Uuid,
	player_id: i32,
	player: Uuid,
	expires_at: DateTime<Utc>,
}

impl JoinRequest {
	fn involves(&self, player: Uuid) -> bool {
		self.host == player || self.player == player
	}

	fn view(&self) -> HostedJoinRequest {
		HostedJoinRequest {
			id: self.id,
			session_id: self.session_id,
			host: self.host,
			player: self.player,
			expires_at: self.expires_at,
		}
	}
}

#[derive(Debug, Default)]
pub(in crate::api) struct JoinRequests {
	pending: HashMap<Uuid, JoinRequest>,
	denied: HashMap<(i32, Uuid), Instant>,
}

pub(in crate::api) type SharedJoinRequests = Arc<RwLock<JoinRequests>>;

impl JoinRequests {
	fn has_pending(&self, player_id: i32, session_id: Uuid) -> bool {
		self.pending.values().any(|request| {
			request.player_id == player_id && request.session_id == session_id
		})
	}

	fn cooldown(&self, player_id: i32, session_id: Uuid) -> Option<Duration> {
		let denied_at = self.denied.get(&(player_id, session_id))?;
		DENY_COOLDOWN.checked_sub(denied_at.elapsed())
	}

	fn insert(&mut self, request: JoinRequest) {
		self.pending.insert(request.id, request);
	}

	fn take_for_host(&mut self, id: Uuid, host_id: i32) -> Option<JoinRequest> {
		if self.pending.get(&id)?.host_id != host_id {
			return None;
		}
		self.pending.remove(&id)
	}

	fn take_where(
		&mut self,
		mut filter: impl FnMut(&JoinRequest) -> bool,
	) -> Vec<JoinRequest> {
		let ids = self
			.pending
			.values()
			.filter(|request| filter(request))
			.map(|request| request.id)
			.collect::<Vec<_>>();
		ids.into_iter()
			.filter_map(|id| self.pending.remove(&id))
			.collect()
	}

	fn record_denial(&mut self, request: &JoinRequest) {
		self.denied
			.retain(|_, denied_at| denied_at.elapsed() < DENY_COOLDOWN);
		self.denied
			.insert((request.player_id, request.session_id), Instant::now());
	}
}

fn request_doc(op: TransformOperation) -> TransformOperation {
	op.id("requestHostedJoin")
		.summary("Ask to join a party member's hosted session")
		.description(
			"Asks the host of a session, who must be in the caller's party and online, \
			 to invite the caller. The host gets `HostedJoinRequested` and the caller \
			 `HostedJoinRequestSent` over the websocket. Requests expire after 60 \
			 seconds. Both sides get `HostedJoinResolved` when the request is allowed, \
			 denied, expires or is cancelled. After a denial the caller must wait 30 \
			 seconds before asking again for the same session.",
		)
		.tag("party")
}

fn allow_doc(op: TransformOperation) -> TransformOperation {
	op.id("allowHostedJoin")
		.summary("Allow a request to join your hosted session")
		.description(
			"Host only. Invites the requester through the normal session invites, \
			 which pushes `SessionInviteReceived`, then resolves the request as \
			 `allowed` with the invite id.",
		)
		.tag("party")
}

fn deny_doc(op: TransformOperation) -> TransformOperation {
	op.id("denyHostedJoin")
		.summary("Deny a request to join your hosted session")
		.tag("party")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/party/sessions/{session_id}/join-requests",
			post_with(self::request, self::request_doc),
		)
		.api_route(
			"/party/join-requests/{request_id}/allow",
			post_with(self::allow, self::allow_doc),
		)
		.api_route(
			"/party/join-requests/{request_id}/deny",
			post_with(self::deny, self::deny_doc),
		)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn request(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(session_id): Path<Uuid>,
) -> Result<(StatusCode, Json<HostedJoinRequest>), PartyError> {
	let request = request_hosted_join(&state, &player, session_id).await?;
	Ok((StatusCode::CREATED, Json(request)))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn allow(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(request_id): Path<Uuid>,
) -> Result<StatusCode, PartyError> {
	respond_to_hosted_join(&state, &player, request_id, true).await?;
	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn deny(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(request_id): Path<Uuid>,
) -> Result<StatusCode, PartyError> {
	respond_to_hosted_join(&state, &player, request_id, false).await?;
	Ok(StatusCode::NO_CONTENT)
}

async fn open_invite_status(
	state: &ApiState,
	session_id: Uuid,
	player_id: i32,
) -> Result<Option<(i32, SessionInviteStatus)>, PartyError> {
	Ok(SessionInvites::find()
		.filter(session_invites::Column::SessionId.eq(session_id))
		.filter(session_invites::Column::RecipientId.eq(player_id))
		.filter(
			session_invites::Column::Status
				.is_in([SessionInviteStatus::Pending, SessionInviteStatus::Accepted]),
		)
		.one(&state.database)
		.await?
		.map(|invite| (invite.id, invite.status)))
}

pub(in crate::api) async fn request_hosted_join(
	state: &ApiState,
	player: &user::Model,
	session_id: Uuid,
) -> Result<HostedJoinRequest, PartyError> {
	if let Some(wait) = state.hosted_join_limit.check(player.id).await {
		return Err(PartyError::RateLimited(wait));
	}

	let _guard = state.party.lock().await;
	let (party, _) = require_party(state, player.id).await?;
	let session = load_session(state, session_id)
		.await
		.map_err(session_error)?;
	if session.host_id == player.id {
		return Err(PartyError::OwnSession);
	}
	if PartyMembers::find_by_id((party.id, session.host_id))
		.one(&state.database)
		.await?
		.is_none()
	{
		return Err(PartyError::HostNotInParty);
	}
	let host = User::find_by_id(session.host_id)
		.one(&state.database)
		.await?
		.ok_or(PartyError::SessionNotFound)?;
	if !connected(state, host.minecraft_uuid).await {
		return Err(PartyError::HostOffline);
	}
	if is_blocked_either_way(state, host.id, player.id).await? {
		return Err(PartyError::Blocked);
	}
	match open_invite_status(state, session.id, player.id).await? {
		Some((_, SessionInviteStatus::Accepted)) => {
			return Err(PartyError::AlreadyInSession);
		}
		Some(_) => return Err(PartyError::AlreadyInvitedToSession),
		None => {}
	}

	let request = {
		let mut requests = state.party.joins.write().await;
		if requests.has_pending(player.id, session.id) {
			return Err(PartyError::JoinRequestPending);
		}
		if let Some(wait) = requests.cooldown(player.id, session.id) {
			return Err(PartyError::RateLimited(wait));
		}

		let request = JoinRequest {
			id: Uuid::now_v7(),
			session_id: session.id,
			party_id: party.id,
			host_id: host.id,
			host: host.minecraft_uuid,
			player_id: player.id,
			player: player.minecraft_uuid,
			expires_at: Utc::now() + LIFETIME,
		};
		requests.insert(request);
		request
	};

	send_to_owner(state, request.host, || {
		ClientBoundPacket::HostedJoinRequested {
			request_id: request.id,
			session_id: request.session_id,
			player: request.player,
			expires_at: request.expires_at,
		}
	})
	.await;
	send_to_owner(state, request.player, || {
		ClientBoundPacket::HostedJoinRequestSent {
			request_id: request.id,
			session_id: request.session_id,
			host: request.host,
			expires_at: request.expires_at,
		}
	})
	.await;

	expire_later(state, request.id);
	Ok(request.view())
}

pub(in crate::api) async fn respond_to_hosted_join(
	state: &ApiState,
	host: &user::Model,
	request_id: Uuid,
	allow: bool,
) -> Result<(), PartyError> {
	let _guard = state.party.lock().await;
	let request = state
		.party
		.joins
		.write()
		.await
		.take_for_host(request_id, host.id)
		.ok_or(PartyError::JoinRequestNotFound)?;

	if !allow {
		state.party.joins.write().await.record_denial(&request);
		resolve(state, &request, HostedJoinResult::Denied, None).await;
		return Ok(());
	}

	match invite_requester(state, host, &request).await {
		Ok(invite_id) => {
			resolve(state, &request, HostedJoinResult::Allowed, Some(invite_id)).await;
			Ok(())
		}
		Err(error) => {
			resolve(state, &request, HostedJoinResult::Cancelled, None).await;
			Err(error)
		}
	}
}

async fn invite_requester(
	state: &ApiState,
	host: &user::Model,
	request: &JoinRequest,
) -> Result<i32, PartyError> {
	let session = load_session(state, request.session_id)
		.await
		.map_err(session_error)?;
	if session.host_id != host.id {
		return Err(PartyError::NotHost);
	}

	let member = User::find_by_id(request.player_id)
		.one(&state.database)
		.await?
		.ok_or(PartyError::MemberNotFound)?;
	let (party, _) = current_party(state, member.id)
		.await?
		.ok_or(PartyError::MemberNotFound)?;
	if party.id != request.party_id
		|| current_party(state, host.id)
			.await?
			.is_none_or(|(party, _)| party.id != request.party_id)
	{
		return Err(PartyError::MemberNotFound);
	}
	if is_blocked_either_way(state, host.id, member.id).await? {
		return Err(PartyError::Blocked);
	}

	if let Some((invite_id, _)) = open_invite_status(state, session.id, member.id).await?
	{
		return Ok(invite_id);
	}

	let friends = are_friends(state, host.id, member.id).await?;
	let invite = invite_to_session(state, host, &session, &member, friends)
		.await
		.map_err(session_error)?;
	Ok(invite.id())
}

async fn resolve(
	state: &ApiState,
	request: &JoinRequest,
	result: HostedJoinResult,
	invite_id: Option<i32>,
) {
	for player in [request.player, request.host] {
		send_to_owner(state, player, || ClientBoundPacket::HostedJoinResolved {
			request_id: request.id,
			session_id: request.session_id,
			result,
			invite_id,
		})
		.await;
	}
}

fn expire_later(state: &ApiState, id: Uuid) {
	let state = state.clone();
	tokio::spawn(async move {
		tokio::time::sleep(LIFETIME).await;

		let expired = state
			.party
			.joins
			.write()
			.await
			.take_where(|request| request.id == id);
		for request in expired {
			resolve(&state, &request, HostedJoinResult::Expired, None).await;
		}
	});
}

async fn cancel_where(state: &ApiState, filter: impl FnMut(&JoinRequest) -> bool) {
	let cancelled = state.party.joins.write().await.take_where(filter);
	for request in cancelled {
		resolve(state, &request, HostedJoinResult::Cancelled, None).await;
	}
}

pub(in crate::api) async fn cancel_hosted_joins_for_session(
	state: &ApiState,
	session_id: Uuid,
) {
	cancel_where(state, |request| request.session_id == session_id).await;
}

pub(super) async fn cancel_hosted_joins_of_player(state: &ApiState, player: Uuid) {
	cancel_where(state, |request| request.involves(player)).await;
}

pub(super) async fn cancel_hosted_joins_in_party(
	state: &ApiState,
	party: Uuid,
	player: Option<Uuid>,
) {
	cancel_where(state, |request| {
		request.party_id == party && player.is_none_or(|player| request.involves(player))
	})
	.await;
}

#[cfg(test)]
mod tests {
	use super::*;

	fn request(player_id: i32, session_id: Uuid) -> JoinRequest {
		JoinRequest {
			id: Uuid::now_v7(),
			session_id,
			party_id: Uuid::nil(),
			host_id: 1,
			host: Uuid::new_v4(),
			player_id,
			player: Uuid::new_v4(),
			expires_at: Utc::now(),
		}
	}

	#[test]
	fn one_pending_request_per_session() {
		let mut requests = JoinRequests::default();
		let session = Uuid::new_v4();
		requests.insert(request(2, session));

		assert!(requests.has_pending(2, session));
		assert!(!requests.has_pending(2, Uuid::new_v4()));
		assert!(!requests.has_pending(3, session));
	}

	#[test]
	fn only_the_host_can_take_a_request() {
		let mut requests = JoinRequests::default();
		let pending = request(2, Uuid::new_v4());
		requests.insert(pending);

		assert!(requests.take_for_host(pending.id, 2).is_none());
		assert!(requests.take_for_host(pending.id, 1).is_some());
		assert!(requests.take_for_host(pending.id, 1).is_none());
	}

	#[test]
	fn denial_starts_a_cooldown_for_that_session() {
		let mut requests = JoinRequests::default();
		let session = Uuid::new_v4();
		requests.record_denial(&request(2, session));

		assert!(requests.cooldown(2, session).is_some());
		assert!(requests.cooldown(2, Uuid::new_v4()).is_none());
		assert!(requests.cooldown(3, session).is_none());
	}

	#[test]
	fn take_where_removes_only_matching_requests() {
		let mut requests = JoinRequests::default();
		let kept = request(2, Uuid::new_v4());
		let taken = request(3, Uuid::new_v4());
		requests.insert(kept);
		requests.insert(taken);

		let removed = requests.take_where(|request| request.involves(taken.player));
		assert_eq!(removed.len(), 1);
		assert_eq!(removed[0].id, taken.id);
		assert!(requests.has_pending(2, kept.session_id));
	}
}
