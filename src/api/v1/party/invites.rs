use std::{
	collections::{HashMap, HashSet},
	time::Duration,
};

use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use chrono::Utc;
use entities::{
	parties, party_invites,
	prelude::*,
	sea_orm_active_enums::{PartyInvitePolicy, PartyInviteStatus, PartyVoiceMode},
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveEnum as _, ActiveValue, ColumnTrait, DbErr, EntityTrait, PaginatorTrait,
	QueryFilter, QueryOrder, QuerySelect, Set, prelude::DateTimeWithTimeZone,
	sea_query::Expr,
};
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use super::{
	PartyError,
	models::{Party, PartyInvite},
	roster::{
		MAX_PARTY_SIZE, add_member, broadcast, create_party, current_party, find_user,
		member_count, move_member, uuids_of,
	},
};
use crate::api::{
	ApiState,
	v0::{
		account::AuthenticatedPlayer,
		social::{are_friends, is_blocked_either_way},
		websocket::{send_to_owner, structs::ClientBoundPacket},
	},
};

const INVITE_TTL: chrono::Duration = chrono::Duration::minutes(10);
const MAX_PENDING_OUTGOING: u64 = 20;
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize, JsonSchema)]
struct InviteBody {
	player: Uuid,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct AcceptBody {
	#[serde(default)]
	leave_current: bool,
}

fn invite_doc(op: TransformOperation) -> TransformOperation {
	op.id("inviteToParty")
		.summary("Invite a friend to your party")
		.description(
			"Invites a friend to the caller's party, creating a party with the caller \
			 as leader first when they are not in one. Only the leader may invite, \
			 unless the party's invite policy is `members`. Invites expire after 10 \
			 minutes. The recipient gets `PartyInviteReceived` over the websocket.",
		)
		.tag("party")
}

fn incoming_doc(op: TransformOperation) -> TransformOperation {
	op.id("listIncomingPartyInvites")
		.summary("List pending party invites sent to you")
		.tag("party")
}

fn outgoing_doc(op: TransformOperation) -> TransformOperation {
	op.id("listOutgoingPartyInvites")
		.summary("List pending party invites you sent")
		.tag("party")
}

fn accept_doc(op: TransformOperation) -> TransformOperation {
	op.id("acceptPartyInvite")
		.summary("Accept a party invite")
		.description(
			"Joins the inviting party. A player already in a party must pass \
			 `leave_current: true` to leave it first. Accepting expires the \
			 player's other pending invites.",
		)
		.tag("party")
}

fn decline_doc(op: TransformOperation) -> TransformOperation {
	op.id("declinePartyInvite")
		.summary("Decline a party invite")
		.tag("party")
}

fn cancel_doc(op: TransformOperation) -> TransformOperation {
	op.id("cancelPartyInvite")
		.summary("Cancel a pending party invite")
		.description("Only the invite's sender or the party leader may cancel it.")
		.tag("party")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/party/invites", post_with(self::invite, self::invite_doc))
		.api_route(
			"/party/invites/incoming",
			get_with(self::incoming, self::incoming_doc),
		)
		.api_route(
			"/party/invites/outgoing",
			get_with(self::outgoing, self::outgoing_doc),
		)
		.api_route(
			"/party/invites/{id}/accept",
			post_with(self::accept, self::accept_doc),
		)
		.api_route(
			"/party/invites/{id}/decline",
			post_with(self::decline, self::decline_doc),
		)
		.api_route(
			"/party/invites/{id}",
			delete_with(self::cancel, self::cancel_doc),
		)
}

async fn views(
	state: &ApiState,
	invites: Vec<party_invites::Model>,
) -> Result<Vec<PartyInvite>, DbErr> {
	let uuids = uuids_of(
		state,
		invites
			.iter()
			.flat_map(|invite| [invite.sender_id, invite.recipient_id]),
	)
	.await?;

	let mut sizes = HashMap::new();
	for party in invites
		.iter()
		.map(|invite| invite.party_id)
		.collect::<HashSet<_>>()
	{
		sizes.insert(party, member_count(state, party).await?);
	}

	Ok(invites
		.into_iter()
		.map(|invite| PartyInvite {
			id: invite.id,
			party_id: invite.party_id,
			sender: uuids.get(&invite.sender_id).copied().unwrap_or_default(),
			recipient: uuids.get(&invite.recipient_id).copied().unwrap_or_default(),
			status: invite.status,
			created_at: invite.created_at.with_timezone(&Utc),
			expires_at: invite.expires_at.with_timezone(&Utc),
			party_size: sizes.get(&invite.party_id).copied().unwrap_or_default(),
		})
		.collect())
}

async fn set_status(
	state: &ApiState,
	invites: &[party_invites::Model],
	status: PartyInviteStatus,
) -> Result<(), DbErr> {
	if invites.is_empty() {
		return Ok(());
	}

	PartyInvites::update_many()
		.col_expr(party_invites::Column::Status, status.as_enum())
		.col_expr(
			party_invites::Column::RespondedAt,
			Expr::value(DateTimeWithTimeZone::from(Utc::now())),
		)
		.filter(
			party_invites::Column::Id
				.is_in(invites.iter().map(|invite| invite.id).collect::<Vec<_>>()),
		)
		.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
		.exec(&state.database)
		.await?;

	Ok(())
}

async fn notify(
	state: &ApiState,
	invites: &[party_invites::Model],
	status: PartyInviteStatus,
	include_recipient: bool,
) -> Result<(), DbErr> {
	let uuids = uuids_of(
		state,
		invites
			.iter()
			.flat_map(|invite| [invite.sender_id, invite.recipient_id]),
	)
	.await?;

	for invite in invites {
		let packet = || ClientBoundPacket::PartyInviteUpdated {
			invite_id: invite.id,
			status,
		};
		if let Some(sender) = uuids.get(&invite.sender_id) {
			send_to_owner(state, *sender, packet).await;
		}
		if include_recipient && let Some(recipient) = uuids.get(&invite.recipient_id) {
			send_to_owner(state, *recipient, packet).await;
		}
	}

	Ok(())
}

async fn transition(
	state: &ApiState,
	invites: &[party_invites::Model],
	status: PartyInviteStatus,
) -> Result<(), DbErr> {
	set_status(state, invites, status).await?;
	let include_recipient = matches!(
		status,
		PartyInviteStatus::Cancelled | PartyInviteStatus::Expired
	);
	notify(state, invites, status, include_recipient).await
}

pub(super) async fn cancel_pending_for_party(
	state: &ApiState,
	party: Uuid,
) -> Result<(), DbErr> {
	let pending = PartyInvites::find()
		.filter(party_invites::Column::PartyId.eq(party))
		.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
		.all(&state.database)
		.await?;
	transition(state, &pending, PartyInviteStatus::Cancelled).await
}

pub(super) async fn cancel_pending_from(
	state: &ApiState,
	party: Uuid,
	sender: i32,
) -> Result<(), DbErr> {
	let pending = PartyInvites::find()
		.filter(party_invites::Column::PartyId.eq(party))
		.filter(party_invites::Column::SenderId.eq(sender))
		.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
		.all(&state.database)
		.await?;
	transition(state, &pending, PartyInviteStatus::Cancelled).await
}

async fn pending_invite(
	state: &ApiState,
	id: Uuid,
) -> Result<party_invites::Model, PartyError> {
	let invite = PartyInvites::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(PartyError::InviteNotFound)?;

	match invite.status {
		PartyInviteStatus::Pending if invite.expires_at <= Utc::now() => {
			transition(
				state,
				std::slice::from_ref(&invite),
				PartyInviteStatus::Expired,
			)
			.await?;
			Err(PartyError::InviteExpired)
		}
		PartyInviteStatus::Pending => Ok(invite),
		PartyInviteStatus::Expired => Err(PartyError::InviteExpired),
		_ => Err(PartyError::InviteNotFound),
	}
}

#[tracing::instrument(level = "debug", skip(state))]
async fn invite(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<InviteBody>,
) -> Result<(StatusCode, Json<PartyInvite>), PartyError> {
	if let Some(wait) = state.party_invite_limit.check(player.id).await {
		return Err(PartyError::RateLimited(wait));
	}

	let _guard = state.party.lock().await;

	let target = find_user(&state, body.player)
		.await?
		.ok_or(PartyError::PlayerNotFound)?;
	if target.id == player.id {
		return Err(PartyError::AlreadyMember);
	}
	if is_blocked_either_way(&state, player.id, target.id).await? {
		return Err(PartyError::Blocked);
	}
	if !are_friends(&state, player.id, target.id).await? {
		return Err(PartyError::NotFriends);
	}

	let party = match current_party(&state, player.id).await? {
		Some((party, _)) => {
			if party.leader_id != player.id
				&& party.invite_policy != PartyInvitePolicy::Members
			{
				return Err(PartyError::NotAllowedToInvite);
			}
			if PartyMembers::find_by_id((party.id, target.id))
				.one(&state.database)
				.await?
				.is_some()
			{
				return Err(PartyError::AlreadyMember);
			}
			Some(party)
		}
		None => None,
	};

	if let Some(party) = &party {
		let existing = PartyInvites::find()
			.filter(party_invites::Column::PartyId.eq(party.id))
			.filter(party_invites::Column::RecipientId.eq(target.id))
			.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
			.one(&state.database)
			.await?;
		if let Some(existing) = existing {
			if existing.expires_at > Utc::now() {
				return Err(PartyError::AlreadyInvited);
			}
			transition(
				&state,
				std::slice::from_ref(&existing),
				PartyInviteStatus::Expired,
			)
			.await?;
		}

		if member_count(&state, party.id).await? >= MAX_PARTY_SIZE {
			return Err(PartyError::PartyFull);
		}
	}

	let outgoing = PartyInvites::find()
		.filter(party_invites::Column::SenderId.eq(player.id))
		.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
		.filter(party_invites::Column::ExpiresAt.gt(Utc::now()))
		.count(&state.database)
		.await?;
	if outgoing >= MAX_PENDING_OUTGOING {
		return Err(PartyError::RateLimited(Duration::from_secs(60)));
	}

	let created = party.is_none();
	let party = match party {
		Some(party) => party,
		None => {
			create_party(
				&state,
				&player,
				PartyVoiceMode::Party,
				PartyInvitePolicy::Leader,
			)
			.await?
		}
	};

	let now = Utc::now();
	let invite = PartyInvites::insert(party_invites::ActiveModel {
		id: Set(Uuid::now_v7()),
		party_id: Set(party.id),
		sender_id: Set(player.id),
		recipient_id: Set(target.id),
		status: Set(PartyInviteStatus::Pending),
		created_at: Set(now.into()),
		expires_at: Set((now + INVITE_TTL).into()),
		responded_at: ActiveValue::NotSet,
	})
	.exec_with_returning(&state.database)
	.await?;

	if created {
		broadcast(&state, party.id).await?;
	}

	let view = views(&state, vec![invite])
		.await?
		.pop()
		.ok_or(PartyError::InviteNotFound)?;
	send_to_owner(&state, target.minecraft_uuid, || {
		ClientBoundPacket::PartyInviteReceived {
			invite: view.clone(),
		}
	})
	.await;

	Ok((StatusCode::CREATED, Json(view)))
}

async fn list_pending(
	state: &ApiState,
	column: party_invites::Column,
	user_id: i32,
) -> Result<Vec<PartyInvite>, PartyError> {
	let invites = PartyInvites::find()
		.filter(column.eq(user_id))
		.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
		.filter(party_invites::Column::ExpiresAt.gt(Utc::now()))
		.order_by_desc(party_invites::Column::CreatedAt)
		.all(&state.database)
		.await?;

	Ok(views(state, invites).await?)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn incoming(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<PartyInvite>>, PartyError> {
	Ok(Json(
		list_pending(&state, party_invites::Column::RecipientId, player.id).await?,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn outgoing(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<PartyInvite>>, PartyError> {
	Ok(Json(
		list_pending(&state, party_invites::Column::SenderId, player.id).await?,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn accept(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<Uuid>,
	body: Option<Json<AcceptBody>>,
) -> Result<Json<Party>, PartyError> {
	let Json(body) = body.unwrap_or_default();
	let _guard = state.party.lock().await;

	let invite = pending_invite(&state, id).await?;
	if invite.recipient_id != player.id {
		return Err(PartyError::InviteNotFound);
	}
	let party = Parties::find_by_id(invite.party_id)
		.one(&state.database)
		.await?
		.ok_or(PartyError::InviteNotFound)?;

	let current = current_party(&state, player.id).await?;
	let already_member = current
		.as_ref()
		.is_some_and(|(current, _)| current.id == party.id);

	if !already_member {
		if current.is_some() && !body.leave_current {
			return Err(PartyError::AlreadyInParty);
		}
		if is_blocked_either_way(&state, invite.sender_id, player.id).await? {
			return Err(PartyError::Blocked);
		}
		if !are_friends(&state, invite.sender_id, player.id).await? {
			return Err(PartyError::NotFriends);
		}
		if member_count(&state, party.id).await? >= MAX_PARTY_SIZE {
			return Err(PartyError::PartyFull);
		}
		match &current {
			Some((current, _)) => move_member(&state, current, &party, &player).await?,
			None => add_member(&state, &party, &player).await?,
		}
	}

	transition(
		&state,
		std::slice::from_ref(&invite),
		PartyInviteStatus::Accepted,
	)
	.await?;

	let others = PartyInvites::find()
		.filter(party_invites::Column::RecipientId.eq(player.id))
		.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
		.filter(party_invites::Column::Id.ne(invite.id))
		.all(&state.database)
		.await?;
	transition(&state, &others, PartyInviteStatus::Expired).await?;

	match broadcast(&state, party.id).await? {
		Some(view) => Ok(Json(view)),
		None => Err(PartyError::InviteNotFound),
	}
}

#[tracing::instrument(level = "debug", skip(state))]
async fn decline(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<Uuid>,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let invite = pending_invite(&state, id).await?;
	if invite.recipient_id != player.id {
		return Err(PartyError::InviteNotFound);
	}

	transition(
		&state,
		std::slice::from_ref(&invite),
		PartyInviteStatus::Declined,
	)
	.await?;
	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn cancel(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<Uuid>,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let invite = PartyInvites::find_by_id(id)
		.one(&state.database)
		.await?
		.filter(|invite| invite.status == PartyInviteStatus::Pending)
		.ok_or(PartyError::InviteNotFound)?;

	if invite.sender_id != player.id {
		let leader = Parties::find_by_id(invite.party_id)
			.select_only()
			.column(parties::Column::LeaderId)
			.into_tuple::<i32>()
			.one(&state.database)
			.await?;
		if leader != Some(player.id) {
			let member = PartyMembers::find_by_id((invite.party_id, player.id))
				.one(&state.database)
				.await?;
			return Err(match member {
				Some(_) => PartyError::NotLeader,
				None => PartyError::InviteNotFound,
			});
		}
	}

	transition(
		&state,
		std::slice::from_ref(&invite),
		PartyInviteStatus::Cancelled,
	)
	.await?;
	Ok(StatusCode::NO_CONTENT)
}

pub(in crate::api) fn spawn_invite_sweeper(state: ApiState) {
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(SWEEP_INTERVAL);
		loop {
			interval.tick().await;

			if let Err(error) = sweep_expired(&state).await {
				warn!("Unable to sweep expired party invites: {error}");
			}
		}
	});
}

async fn sweep_expired(state: &ApiState) -> Result<(), DbErr> {
	let expired = PartyInvites::find()
		.filter(party_invites::Column::Status.eq(PartyInviteStatus::Pending))
		.filter(party_invites::Column::ExpiresAt.lte(Utc::now()))
		.all(&state.database)
		.await?;
	if expired.is_empty() {
		return Ok(());
	}

	transition(state, &expired, PartyInviteStatus::Expired).await?;
	info!(invites = expired.len(), "Expired party invites");

	Ok(())
}
