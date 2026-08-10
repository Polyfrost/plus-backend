use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, patch_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, Query, State},
	http::StatusCode,
};
use chrono::{DateTime, FixedOffset, Utc};
use entities::sea_orm_active_enums::SessionInviteStatus;
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
	Set,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::account::AuthenticatedPlayer,
	v0::groups::{GroupError, enforce_special_chat_cooldown, load_group, member_ids, require_membership},
	v0::social::is_blocked_either_way,
	v0::websocket::{send_to_owner, structs::ClientBoundPacket},
};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionInviteInfo {
	id: i32,
	session_id: Uuid,
	status: SessionInviteStatus,
}

async fn load_invite_info(
	state: &ApiState,
	session_invite_id: Option<i32>,
) -> Result<Option<SessionInviteInfo>, GroupError> {
	use entities::prelude::*;

	let Some(id) = session_invite_id else {
		return Ok(None);
	};

	Ok(SessionInvites::find_by_id(id)
		.one(&state.database)
		.await?
		.map(|invite| SessionInviteInfo {
			id: invite.id,
			session_id: invite.session_id,
			status: invite.status,
		}))
}

async fn require_not_blocked_by_members(
	state: &ApiState,
	group_id: i32,
	player_id: i32,
) -> Result<(), GroupError> {
	let others = member_ids(state, group_id)
		.await?
		.into_iter()
		.filter(|id| *id != player_id);

	for other in others {
		if is_blocked_either_way(state, player_id, other).await? {
			return Err(GroupError::Blocked);
		}
	}

	Ok(())
}

const MAX_MESSAGE_LENGTH: usize = 4000;
const DEFAULT_PAGE_SIZE: u64 = 50;
const MAX_PAGE_SIZE: u64 = 200;

#[derive(Debug, Serialize, JsonSchema)]
pub struct GroupMessage {
	id: i64,
	sender: Uuid,
	content: String,
	sent_at: DateTime<FixedOffset>,
	edited_at: Option<DateTime<FixedOffset>>,
	session_invite: Option<SessionInviteInfo>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendMessageRequest {
	content: String,
	/// Optional client-generated key to make retried sends idempotent.
	idempotency_key: Option<Uuid>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EditMessageRequest {
	content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HistoryQuery {
	/// Return messages sent before this message id (for pagination).
	before: Option<i64>,
	limit: Option<u64>,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listGroupMessages")
		.summary("List messages in a DM or group chat")
		.tag("groups")
}

fn send_doc(op: TransformOperation) -> TransformOperation {
	op.id("sendGroupMessage")
		.summary("Send a message to a DM or group chat")
		.tag("groups")
}

fn edit_doc(op: TransformOperation) -> TransformOperation {
	op.id("editGroupMessage")
		.summary("Edit a message you sent")
		.tag("groups")
}

fn delete_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteGroupMessage")
		.summary("Delete a message you sent")
		.tag("groups")
}

fn read_doc(op: TransformOperation) -> TransformOperation {
	op.id("markGroupRead")
		.summary("Mark a group chat as read up to a message")
		.tag("groups")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/{id}/messages", get_with(self::list, self::list_doc))
		.api_route("/{id}/messages", post_with(self::send, self::send_doc))
		.api_route(
			"/{id}/messages/{message_id}",
			patch_with(self::edit, self::edit_doc),
		)
		.api_route(
			"/{id}/messages/{message_id}",
			delete_with(self::delete, self::delete_doc),
		)
		.api_route("/{id}/read/{message_id}", post_with(self::mark_read, self::read_doc))
}

async fn notify_other_members(
	state: &ApiState,
	group_id: i32,
	exclude: i32,
	make_packet: impl Fn() -> ClientBoundPacket,
) -> Result<(), sea_orm::DbErr> {
	use entities::{prelude::*, user};

	let ids = member_ids(state, group_id)
		.await?
		.into_iter()
		.filter(|id| *id != exclude)
		.collect::<Vec<_>>();

	let recipients = User::find()
		.filter(user::Column::Id.is_in(ids))
		.all(&state.database)
		.await?;

	for recipient in recipients {
		send_to_owner(state, recipient.minecraft_uuid, &make_packet).await;
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
	Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<GroupMessage>>, GroupError> {
	use entities::{group_messages, prelude::*, user};
	use sea_orm::QueryOrder;

	require_membership(&state, id, player.id).await?;

	let limit = query.limit.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE);

	let mut select = GroupMessages::find()
		.filter(group_messages::Column::GroupId.eq(id))
		.filter(group_messages::Column::DeletedAt.is_null());
	if let Some(before) = query.before {
		select = select.filter(group_messages::Column::Id.lt(before));
	}

	let rows = select
		.order_by_desc(group_messages::Column::Id)
		.limit(limit)
		.all(&state.database)
		.await?;

	let sender_ids = rows.iter().map(|row| row.sender_id).collect::<Vec<_>>();
	let senders = User::find()
		.filter(user::Column::Id.is_in(sender_ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect::<std::collections::HashMap<_, _>>();

	let invite_ids = rows
		.iter()
		.filter_map(|row| row.session_invite_id)
		.collect::<Vec<_>>();
	let invites = if invite_ids.is_empty() {
		std::collections::HashMap::new()
	} else {
		SessionInvites::find()
			.filter(entities::session_invites::Column::Id.is_in(invite_ids))
			.all(&state.database)
			.await?
			.into_iter()
			.map(|invite| {
				(
					invite.id,
					SessionInviteInfo {
						id: invite.id,
						session_id: invite.session_id,
						status: invite.status,
					},
				)
			})
			.collect::<std::collections::HashMap<_, _>>()
	};

	Ok(Json(
		rows.into_iter()
			.filter_map(|row| {
				let session_invite = row.session_invite_id.and_then(|id| invites.get(&id)).cloned();
				senders.get(&row.sender_id).map(|uuid| GroupMessage {
					id: row.id,
					sender: *uuid,
					content: row.content,
					sent_at: row.sent_at,
					edited_at: row.edited_at,
					session_invite,
				})
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn send(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
	Json(body): Json<SendMessageRequest>,
) -> Result<(StatusCode, Json<GroupMessage>), GroupError> {
	use entities::{group_messages, prelude::*};

	require_membership(&state, id, player.id).await?;
	let group = load_group(&state, id).await?;
	require_not_blocked_by_members(&state, id, player.id).await?;

	let content = body.content.trim();
	if content.is_empty() || content.len() > MAX_MESSAGE_LENGTH {
		return Err(GroupError::InvalidContent);
	}

	if let Some(key) = body.idempotency_key
		&& let Some(existing) = GroupMessages::find()
			.filter(group_messages::Column::GroupId.eq(id))
			.filter(group_messages::Column::IdempotencyKey.eq(key))
			.one(&state.database)
			.await?
	{
		return Ok((
			StatusCode::OK,
			Json(GroupMessage {
				id: existing.id,
				sender: player.minecraft_uuid,
				content: existing.content,
				sent_at: existing.sent_at,
				edited_at: existing.edited_at,
				session_invite: None,
			}),
		));
	}

	enforce_special_chat_cooldown(&state, &group, player.id).await?;

	let message = GroupMessages::insert(group_messages::ActiveModel {
		group_id: Set(id),
		sender_id: Set(player.id),
		content: Set(content.to_string()),
		sent_at: ActiveValue::NotSet,
		edited_at: Set(None),
		deleted_at: Set(None),
		idempotency_key: Set(body.idempotency_key),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	notify_other_members(&state, id, player.id, || ClientBoundPacket::GroupMessageReceived {
		group_id: id,
		message_id: message.id,
		sender: player.minecraft_uuid,
		content: message.content.clone(),
		session_invite_id: None,
		session_invite_status: None,
	})
	.await?;

	Ok((
		StatusCode::CREATED,
		Json(GroupMessage {
			id: message.id,
			sender: player.minecraft_uuid,
			content: message.content,
			sent_at: message.sent_at,
			edited_at: message.edited_at,
			session_invite: None,
		}),
	))
}

async fn load_own_message(
	state: &ApiState,
	group_id: i32,
	message_id: i64,
	sender_id: i32,
) -> Result<entities::group_messages::Model, GroupError> {
	use entities::prelude::*;

	let message = GroupMessages::find_by_id(message_id)
		.one(&state.database)
		.await?
		.filter(|message| message.group_id == group_id && message.deleted_at.is_none())
		.ok_or(GroupError::MessageMissing)?;

	if message.sender_id != sender_id {
		return Err(GroupError::MessageForbidden);
	}

	Ok(message)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn edit(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path((id, message_id)): Path<(i32, i64)>,
	Json(body): Json<EditMessageRequest>,
) -> Result<Json<GroupMessage>, GroupError> {
	require_membership(&state, id, player.id).await?;
	let message = load_own_message(&state, id, message_id, player.id).await?;
	require_not_blocked_by_members(&state, id, player.id).await?;

	let content = body.content.trim();
	if content.is_empty() || content.len() > MAX_MESSAGE_LENGTH {
		return Err(GroupError::InvalidContent);
	}

	let session_invite_id = message.session_invite_id;
	let mut active: entities::group_messages::ActiveModel = message.into();
	active.content = Set(content.to_string());
	active.edited_at = Set(Some(Utc::now().into()));
	let message = active.update(&state.database).await?;

	notify_other_members(&state, id, player.id, || ClientBoundPacket::GroupMessageEdited {
		group_id: id,
		message_id: message.id,
		content: message.content.clone(),
		session_invite_status: None,
	})
	.await?;

	Ok(Json(GroupMessage {
		id: message.id,
		sender: player.minecraft_uuid,
		content: message.content,
		sent_at: message.sent_at,
		edited_at: message.edited_at,
		session_invite: load_invite_info(&state, session_invite_id).await?,
	}))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn delete(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path((id, message_id)): Path<(i32, i64)>,
) -> Result<StatusCode, GroupError> {
	let message = load_own_message(&state, id, message_id, player.id).await?;

	let mut active: entities::group_messages::ActiveModel = message.into();
	active.deleted_at = Set(Some(Utc::now().into()));
	active.content = Set(String::new());
	active.update(&state.database).await?;

	notify_other_members(&state, id, player.id, || ClientBoundPacket::GroupMessageDeleted {
		group_id: id,
		message_id,
	})
	.await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn mark_read(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path((id, message_id)): Path<(i32, i64)>,
) -> Result<StatusCode, GroupError> {
	use entities::{group_message_reads, prelude::*};
	use sea_orm::sea_query::OnConflict;

	require_membership(&state, id, player.id).await?;

	// Clients may send an id that doesn't correspond to a real row (e.g. a client-local optimistic/
	// pending message id used before the server has acked a send). last_read_message_id has a FK to
	// group_messages, so inserting an unknown id would 500. Clamp to the newest real message in this
	// group instead of trusting the client-supplied id verbatim.
	let message_id = match entities::prelude::GroupMessages::find_by_id(message_id)
		.filter(entities::group_messages::Column::GroupId.eq(id))
		.one(&state.database)
		.await?
	{
		Some(_) => Some(message_id),
		None => {
			entities::prelude::GroupMessages::find()
				.filter(entities::group_messages::Column::GroupId.eq(id))
				.order_by_desc(entities::group_messages::Column::Id)
				.select_only()
				.column(entities::group_messages::Column::Id)
				.into_tuple::<i64>()
				.one(&state.database)
				.await?
		}
	};

	let Some(message_id) = message_id else {
		return Ok(StatusCode::NO_CONTENT);
	};

	GroupMessageReads::insert(group_message_reads::ActiveModel {
		group_id: Set(id),
		user_id: Set(player.id),
		last_read_message_id: Set(Some(message_id)),
		updated_at: ActiveValue::NotSet,
	})
	.on_conflict(
		OnConflict::columns([
			group_message_reads::Column::GroupId,
			group_message_reads::Column::UserId,
		])
		.update_columns([
			group_message_reads::Column::LastReadMessageId,
			group_message_reads::Column::UpdatedAt,
		])
		.to_owned(),
	)
	.exec(&state.database)
	.await?;

	Ok(StatusCode::NO_CONTENT)
}
