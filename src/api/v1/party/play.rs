use std::{collections::HashMap, sync::Arc, time::Duration};

use aide::{
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use chrono::Utc;
use entities::{
	prelude::*, sea_orm_active_enums::SessionInviteStatus, session_invites, user,
};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter};
use tokio::sync::RwLock;
use tracing::warn;
use uuid::Uuid;

use super::{
	PartyError,
	models::{PlayDestination, PlayEndReason, PlayReason, PlayResponse},
	roster::{current_party, member_uuids, members_of, require_leader, require_party},
};
use crate::api::{
	ApiState,
	v0::{
		account::AuthenticatedPlayer,
		sessions::{SessionError, SessionInvite, invite_to_session, load_session},
		social::{are_friends, is_blocked_either_way},
		websocket::{
			send_to_owner,
			structs::{ClientBoundPacket, WebsocketError},
		},
	},
};

const COUNTDOWN: chrono::Duration = chrono::Duration::seconds(5);
const LIFETIME: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Clone, Copy)]
pub(in crate::api) struct ActivePlay {
	id: Uuid,
	session_id: Option<Uuid>,
}

pub(in crate::api) type ActivePlays = Arc<RwLock<HashMap<Uuid, ActivePlay>>>;

fn invite_doc(op: TransformOperation) -> TransformOperation {
	op.id("invitePartyToSession")
		.summary("Invite your whole party to your hosted session")
		.description(
			"Leader only, and the caller must host the session. Invites every other \
			 party member through the normal session invites, friends or not, \
			 skipping blocked pairs and members who already hold an open invite to \
			 the session. Links the session to the party's active Play together, \
			 which pushes `PlayTogetherUpdated` with the `session_id`.",
		)
		.tag("party")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/party/sessions/{session_id}/invite",
		post_with(self::invite_party, self::invite_doc),
	)
}

async fn send_all(
	state: &ApiState,
	players: &[Uuid],
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	for player in players {
		send_to_owner(state, *player, &mut make_packet).await;
	}
}

pub(super) async fn end(
	state: &ApiState,
	party: Uuid,
	reason: PlayEndReason,
	players: &[Uuid],
) {
	let Some(play) = state.party.plays.write().await.remove(&party) else {
		return;
	};
	send_all(state, players, || ClientBoundPacket::PlayTogetherEnded {
		id: play.id,
		reason,
	})
	.await;
}

fn expire_later(state: &ApiState, party: Uuid, id: Uuid) {
	let state = state.clone();
	tokio::spawn(async move {
		tokio::time::sleep(LIFETIME).await;

		{
			let mut plays = state.party.plays.write().await;
			if plays.get(&party).is_none_or(|play| play.id != id) {
				return;
			}
			plays.remove(&party);
		}

		match member_uuids(&state, party).await {
			Ok(players) => {
				send_all(&state, &players, || ClientBoundPacket::PlayTogetherEnded {
					id,
					reason: PlayEndReason::Expired,
				})
				.await;
			}
			Err(error) => warn!("Unable to announce expired Play together {id}: {error}"),
		}
	});
}

pub(in crate::api) async fn start_play_together(
	state: &ApiState,
	player: &user::Model,
	destination: PlayDestination,
) -> Result<(), WebsocketError> {
	destination
		.validate()
		.map_err(WebsocketError::InvalidPlayTogether)?;

	let _guard = state.party.lock().await;
	let (party, _) = current_party(state, player.id)
		.await?
		.ok_or(WebsocketError::NotInParty)?;
	if party.leader_id != player.id {
		return Err(WebsocketError::NotPartyLeader);
	}
	if state.play_together_limit.check(player.id).await.is_some() {
		return Err(WebsocketError::RateLimited);
	}

	let players = member_uuids(state, party.id).await?;
	let id = Uuid::now_v7();
	let previous = state.party.plays.write().await.insert(
		party.id,
		ActivePlay {
			id,
			session_id: None,
		},
	);
	if let Some(previous) = previous {
		send_all(state, &players, || ClientBoundPacket::PlayTogetherEnded {
			id: previous.id,
			reason: PlayEndReason::Replaced,
		})
		.await;
	}

	let starts_at = Utc::now() + COUNTDOWN;
	let others = players
		.iter()
		.copied()
		.filter(|other| *other != player.minecraft_uuid)
		.collect::<Vec<_>>();
	send_all(state, &others, || ClientBoundPacket::PlayTogetherStarted {
		id,
		party_id: party.id,
		leader: player.minecraft_uuid,
		destination: destination.clone(),
		starts_at,
		session_id: None,
	})
	.await;
	send_all(state, &players, || ClientBoundPacket::PlayTogetherUpdated {
		id,
		player: player.minecraft_uuid,
		response: PlayResponse::Joining,
		reason: None,
		session_id: None,
	})
	.await;

	expire_later(state, party.id, id);
	Ok(())
}

pub(in crate::api) async fn cancel_play_together(
	state: &ApiState,
	player: &user::Model,
	id: Uuid,
) -> Result<(), WebsocketError> {
	let _guard = state.party.lock().await;
	let (party, _) = current_party(state, player.id)
		.await?
		.ok_or(WebsocketError::NotInParty)?;
	if party.leader_id != player.id {
		return Err(WebsocketError::NotPartyLeader);
	}
	if state
		.party
		.plays
		.read()
		.await
		.get(&party.id)
		.is_none_or(|play| play.id != id)
	{
		return Err(WebsocketError::PlayTogetherInactive);
	}

	let players = member_uuids(state, party.id).await?;
	end(state, party.id, PlayEndReason::Cancelled, &players).await;
	Ok(())
}

pub(in crate::api) async fn respond_to_play_together(
	state: &ApiState,
	player: &user::Model,
	id: Uuid,
	response: PlayResponse,
	reason: Option<PlayReason>,
) -> Result<(), WebsocketError> {
	if state.play_response_limit.check(player.id).await.is_some() {
		return Err(WebsocketError::RateLimited);
	}

	let (party, _) = current_party(state, player.id)
		.await?
		.ok_or(WebsocketError::NotInParty)?;
	let session_id = match state.party.plays.read().await.get(&party.id) {
		Some(play) if play.id == id => play.session_id,
		_ => return Err(WebsocketError::PlayTogetherInactive),
	};

	let players = member_uuids(state, party.id).await?;
	send_all(state, &players, || ClientBoundPacket::PlayTogetherUpdated {
		id,
		player: player.minecraft_uuid,
		response,
		reason,
		session_id,
	})
	.await;
	Ok(())
}

async fn link_session(
	state: &ApiState,
	party: Uuid,
	leader: Uuid,
	session_id: Uuid,
) -> Result<(), DbErr> {
	let id = {
		let mut plays = state.party.plays.write().await;
		let Some(play) = plays.get_mut(&party) else {
			return Ok(());
		};
		play.session_id = Some(session_id);
		play.id
	};

	let players = member_uuids(state, party).await?;
	send_all(state, &players, || ClientBoundPacket::PlayTogetherUpdated {
		id,
		player: leader,
		response: PlayResponse::Joining,
		reason: None,
		session_id: Some(session_id),
	})
	.await;
	Ok(())
}

fn session_error(error: SessionError) -> PartyError {
	match error {
		SessionError::SessionMissing | SessionError::SessionExpired => {
			PartyError::SessionNotFound
		}
		SessionError::Database(error) => PartyError::Database(error),
		error => PartyError::Session(error),
	}
}

#[tracing::instrument(level = "debug", skip(state))]
async fn invite_party(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(session_id): Path<Uuid>,
) -> Result<(StatusCode, Json<Vec<SessionInvite>>), PartyError> {
	let _guard = state.party.lock().await;

	let (party, _) = require_party(&state, player.id).await?;
	require_leader(&party, &player)?;
	let session = load_session(&state, session_id)
		.await
		.map_err(session_error)?;
	if session.host_id != player.id {
		return Err(PartyError::NotHost);
	}

	let invited = SessionInvites::find()
		.filter(session_invites::Column::SessionId.eq(session.id))
		.filter(
			session_invites::Column::Status
				.is_in([SessionInviteStatus::Pending, SessionInviteStatus::Accepted]),
		)
		.all(&state.database)
		.await?
		.into_iter()
		.map(|invite| invite.recipient_id)
		.collect::<Vec<_>>();

	let mut invites = Vec::new();
	for (_, member) in members_of(&state.database, party.id).await? {
		if member.id == player.id || invited.contains(&member.id) {
			continue;
		}
		if is_blocked_either_way(&state, player.id, member.id).await? {
			continue;
		}
		let friends = are_friends(&state, player.id, member.id).await?;
		invites.push(
			invite_to_session(&state, &player, &session, &member, friends)
				.await
				.map_err(session_error)?,
		);
	}

	link_session(&state, party.id, player.minecraft_uuid, session.id).await?;

	Ok((StatusCode::CREATED, Json(invites)))
}
