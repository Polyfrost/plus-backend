use aide::{
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use chrono::{DateTime, FixedOffset, Utc};
use entities::sea_orm_active_enums::SessionInviteStatus;
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set,
};
use serde::Serialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::{
		account::AuthenticatedPlayer,
		groups::get_or_create_dm_group,
		sessions::{SessionError, find_user_by_uuid, load_session},
		social::{are_friends, is_blocked_either_way},
		websocket::{send_to_owner, structs::ClientBoundPacket},
	},
};

async fn post_invite_message(
	state: &ApiState,
	sender: &entities::user::Model,
	recipient: &entities::user::Model,
	invite_id: i32,
) -> Result<(), SessionError> {
	use entities::{group_messages, prelude::*};

	let (_, dm_group) = get_or_create_dm_group(state, sender.id, recipient.id).await?;

	let message = GroupMessages::insert(group_messages::ActiveModel {
		group_id: Set(dm_group.id),
		sender_id: Set(sender.id),
		content: Set("Invited you to their world".to_string()),
		sent_at: ActiveValue::NotSet,
		edited_at: Set(None),
		deleted_at: Set(None),
		idempotency_key: Set(None),
		session_invite_id: Set(Some(invite_id)),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	send_to_owner(state, recipient.minecraft_uuid, || {
		ClientBoundPacket::GroupMessageReceived {
			group_id: dm_group.id,
			message_id: message.id,
			sender: sender.minecraft_uuid,
			content: message.content.clone(),
			session_invite_id: Some(invite_id),
			session_invite_status: Some(SessionInviteStatus::Pending),
		}
	})
	.await;

	Ok(())
}

pub(super) async fn expire_invites_for_sessions(
	state: &ApiState,
	session_ids: Vec<Uuid>,
) -> Result<usize, SessionError> {
	use entities::{prelude::*, session_invites, user};
	use sea_orm::{ActiveEnum as _, prelude::DateTimeWithTimeZone, sea_query::Expr};

	if session_ids.is_empty() {
		return Ok(0);
	}

	let invites = SessionInvites::find()
		.filter(session_invites::Column::SessionId.is_in(session_ids))
		.filter(session_invites::Column::Status.eq(SessionInviteStatus::Pending))
		.all(&state.database)
		.await?;
	if invites.is_empty() {
		return Ok(0);
	}

	SessionInvites::update_many()
		.col_expr(
			session_invites::Column::Status,
			SessionInviteStatus::Expired.as_enum(),
		)
		.col_expr(
			session_invites::Column::RespondedAt,
			Expr::value(DateTimeWithTimeZone::from(Utc::now())),
		)
		.filter(
			session_invites::Column::Id
				.is_in(invites.iter().map(|invite| invite.id).collect::<Vec<_>>()),
		)
		.exec(&state.database)
		.await?;

	let recipients = User::find()
		.filter(
			user::Column::Id.is_in(
				invites
					.iter()
					.map(|invite| invite.recipient_id)
					.collect::<Vec<_>>(),
			),
		)
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect::<std::collections::HashMap<_, _>>();

	for invite in &invites {
		if let Some(&recipient) = recipients.get(&invite.recipient_id) {
			send_to_owner(state, recipient, || {
				ClientBoundPacket::SessionInviteUpdated {
					invite_id: invite.id,
					status: "expired".to_string(),
				}
			})
			.await;
		}

		notify_invite_message_status(state, invite.id, SessionInviteStatus::Expired)
			.await?;
	}

	Ok(invites.len())
}

async fn find_invite_message(
	state: &ApiState,
	invite_id: i32,
) -> Result<Option<(i32, i64)>, SessionError> {
	use entities::{group_messages, prelude::*};

	Ok(GroupMessages::find()
		.filter(group_messages::Column::SessionInviteId.eq(invite_id))
		.one(&state.database)
		.await?
		.map(|message| (message.group_id, message.id)))
}

async fn notify_invite_message_status(
	state: &ApiState,
	invite_id: i32,
	status: SessionInviteStatus,
) -> Result<(), SessionError> {
	use entities::{group_members, prelude::*};

	let Some((group_id, message_id)) = find_invite_message(state, invite_id).await?
	else {
		return Ok(());
	};

	let member_ids = GroupMembers::find()
		.filter(group_members::Column::GroupId.eq(group_id))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|member| member.user_id)
		.collect::<Vec<_>>();

	let members = User::find()
		.filter(entities::user::Column::Id.is_in(member_ids))
		.all(&state.database)
		.await?;

	for member in members {
		let status = status.clone();
		send_to_owner(state, member.minecraft_uuid, move || {
			ClientBoundPacket::GroupMessageEdited {
				group_id,
				message_id,
				content: "Invited you to their world".to_string(),
				session_invite_status: Some(status.clone()),
			}
		})
		.await;
	}

	Ok(())
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionInvite {
	id: i32,
	session_id: Uuid,
	sender: Uuid,
	eos_product_user_id: Option<String>,
	eos_session_id: Option<String>,
	created_at: DateTime<FixedOffset>,
}

fn send_doc(op: TransformOperation) -> TransformOperation {
	op.id("sendSessionInvite")
		.summary("Invite a friend to a hosted multiplayer session")
		.description(
			"Sends a multiplayer session invite to a friend, routed through \
			 the Friends System. The recipient is pushed the invite over the \
			 realtime connection; joining happens over EOS's own session/P2P \
			 services using the session's eos_session_id.",
		)
		.tag("sessions")
}

fn incoming_doc(op: TransformOperation) -> TransformOperation {
	op.id("listIncomingSessionInvites")
		.summary("List incoming multiplayer session invites")
		.tag("sessions")
}

fn accept_doc(op: TransformOperation) -> TransformOperation {
	op.id("acceptSessionInvite")
		.summary("Accept a multiplayer session invite")
		.tag("sessions")
}

fn decline_doc(op: TransformOperation) -> TransformOperation {
	op.id("declineSessionInvite")
		.summary("Decline a multiplayer session invite")
		.tag("sessions")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/{id}/invites/{player}",
			post_with(self::send, self::send_doc),
		)
		.api_route(
			"/invites/incoming",
			get_with(self::incoming, self::incoming_doc),
		)
		.api_route(
			"/invites/{invite_id}/accept",
			post_with(self::accept, self::accept_doc),
		)
		.api_route(
			"/invites/{invite_id}/decline",
			post_with(self::decline, self::decline_doc),
		)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn send(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path((session_id, target)): Path<(Uuid, Uuid)>,
) -> Result<(StatusCode, Json<SessionInvite>), SessionError> {
	use entities::{prelude::*, session_invites};

	let session = load_session(&state, session_id).await?;
	if session.host_id != player.id {
		return Err(SessionError::NotHost);
	}

	let target = find_user_by_uuid(&state, target).await?;
	if target.id == player.id {
		return Err(SessionError::SelfTarget);
	}
	if is_blocked_either_way(&state, player.id, target.id).await? {
		return Err(SessionError::Blocked);
	}
	if !are_friends(&state, player.id, target.id).await? {
		return Err(SessionError::NotFriends);
	}

	let invite = SessionInvites::insert(session_invites::ActiveModel {
		session_id: Set(session.id),
		sender_id: Set(player.id),
		recipient_id: Set(target.id),
		status: Set(SessionInviteStatus::Pending),
		created_at: ActiveValue::NotSet,
		responded_at: Set(None),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	send_to_owner(&state, target.minecraft_uuid, || {
		ClientBoundPacket::SessionInviteReceived {
			invite_id: invite.id,
			session_id: session.id,
			sender: player.minecraft_uuid,
		}
	})
	.await;

	post_invite_message(&state, &player, &target, invite.id).await?;

	Ok((
		StatusCode::CREATED,
		Json(SessionInvite {
			id: invite.id,
			session_id: session.id,
			sender: player.minecraft_uuid,
			eos_product_user_id: player.eos_product_user_id,
			eos_session_id: session.eos_session_id,
			created_at: invite.created_at,
		}),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn incoming(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<SessionInvite>>, SessionError> {
	use entities::{prelude::*, session_invites, user};

	let rows = SessionInvites::find()
		.filter(session_invites::Column::RecipientId.eq(player.id))
		.filter(session_invites::Column::Status.eq(SessionInviteStatus::Pending))
		.all(&state.database)
		.await?;

	let session_ids = rows.iter().map(|row| row.session_id).collect::<Vec<_>>();
	let sessions = GameSessions::find()
		.filter(entities::game_sessions::Column::Id.is_in(session_ids))
		.filter(entities::game_sessions::Column::ExpiresAt.gt(Utc::now()))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|session| (session.id, session))
		.collect::<std::collections::HashMap<_, _>>();

	let sender_ids = rows.iter().map(|row| row.sender_id).collect::<Vec<_>>();
	let senders = User::find()
		.filter(user::Column::Id.is_in(sender_ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user))
		.collect::<std::collections::HashMap<_, _>>();

	Ok(Json(
		rows.into_iter()
			.filter_map(|row| {
				let sender = senders.get(&row.sender_id)?;
				let session = sessions.get(&row.session_id)?;
				Some(SessionInvite {
					id: row.id,
					session_id: row.session_id,
					sender: sender.minecraft_uuid,
					eos_product_user_id: sender.eos_product_user_id.clone(),
					eos_session_id: session.eos_session_id.clone(),
					created_at: row.created_at,
				})
			})
			.collect(),
	))
}

async fn load_pending_invite(
	state: &ApiState,
	id: i32,
) -> Result<entities::session_invites::Model, SessionError> {
	use entities::prelude::*;

	SessionInvites::find_by_id(id)
		.one(&state.database)
		.await?
		.filter(|invite| invite.status == SessionInviteStatus::Pending)
		.ok_or(SessionError::InviteMissing)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn accept(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, SessionError> {
	use entities::prelude::*;

	let invite = load_pending_invite(&state, id).await?;
	if invite.recipient_id != player.id {
		return Err(SessionError::InviteForbidden);
	}
	load_session(&state, invite.session_id).await?;

	let mut active: entities::session_invites::ActiveModel = invite.clone().into();
	active.status = Set(SessionInviteStatus::Accepted);
	active.responded_at = Set(Some(Utc::now().into()));
	active.update(&state.database).await?;

	if let Some(sender) = User::find_by_id(invite.sender_id)
		.one(&state.database)
		.await?
	{
		send_to_owner(&state, sender.minecraft_uuid, || {
			ClientBoundPacket::SessionInviteUpdated {
				invite_id: invite.id,
				status: "accepted".to_string(),
			}
		})
		.await;
	}

	notify_invite_message_status(&state, invite.id, SessionInviteStatus::Accepted)
		.await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn decline(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, SessionError> {
	use entities::prelude::*;

	let invite = load_pending_invite(&state, id).await?;
	if invite.recipient_id != player.id {
		return Err(SessionError::InviteForbidden);
	}

	let mut active: entities::session_invites::ActiveModel = invite.clone().into();
	active.status = Set(SessionInviteStatus::Declined);
	active.responded_at = Set(Some(Utc::now().into()));
	active.update(&state.database).await?;

	if let Some(sender) = User::find_by_id(invite.sender_id)
		.one(&state.database)
		.await?
	{
		send_to_owner(&state, sender.minecraft_uuid, || {
			ClientBoundPacket::SessionInviteUpdated {
				invite_id: invite.id,
				status: "declined".to_string(),
			}
		})
		.await;
	}

	notify_invite_message_status(&state, invite.id, SessionInviteStatus::Declined)
		.await?;

	Ok(StatusCode::NO_CONTENT)
}
