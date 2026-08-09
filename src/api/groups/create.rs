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
use chrono::{DateTime, FixedOffset};
use entities::sea_orm_active_enums::{GroupKind, GroupMemberRole};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	account::AuthenticatedPlayer,
	groups::{GroupError, find_user_by_uuid, load_group, member_ids, require_membership},
	social::{are_friends, is_blocked_either_way},
};

#[derive(Debug, Serialize, JsonSchema)]
pub struct GroupSummary {
	id: i32,
	kind: GroupKind,
	name: Option<String>,
	members: Vec<Uuid>,
	last_message: Option<LastMessage>,
	unread: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LastMessage {
	content: String,
	sender: Uuid,
	sent_at: DateTime<FixedOffset>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateGroupRequest {
	name: String,
	members: Vec<Uuid>,
}

fn dm_doc(op: TransformOperation) -> TransformOperation {
	op.id("openDirectMessage")
		.summary("Open (or fetch) a direct message with a friend")
		.description(
			"Returns the existing DM with the given player, or creates one. \
			 The two players must be friends and neither may have blocked the \
			 other.",
		)
		.tag("groups")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createGroup")
		.summary("Create a group chat")
		.tag("groups")
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listGroups")
		.summary("List direct messages and group chats")
		.description("Lists every DM and group chat the authenticated player belongs to.")
		.tag("groups")
}

fn claim_doc(op: TransformOperation) -> TransformOperation {
	op.id("claimSpecialChatGroup")
		.summary("Convert the Special Chat group into a normal, player-owned group chat")
		.description(
			"Claims ownership of the eagerly-created Special Chat group (see \
			 GET /special-chat), turning it into an ordinary group chat you \
			 own — a fresh Special Chat group is created the next time one is \
			 needed. Only works on that specific group.",
		)
		.tag("groups")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/dm/{player}", post_with(self::open_dm, self::dm_doc))
		.api_route("/", post_with(self::create_group, self::create_doc))
		.api_route("/", get_with(self::list, self::list_doc))
		.api_route("/{id}/claim", post_with(self::claim, self::claim_doc))
}

async fn summarize(
	state: &ApiState,
	group: entities::groups::Model,
	viewer_id: i32,
) -> Result<GroupSummary, GroupError> {
	use entities::{group_message_reads, group_messages, prelude::*, user};
	use sea_orm::QueryOrder;

	let ids = member_ids(state, group.id).await?;
	let members = User::find()
		.filter(user::Column::Id.is_in(ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| user.minecraft_uuid)
		.collect();

	let last_message = GroupMessages::find()
		.filter(group_messages::Column::GroupId.eq(group.id))
		.filter(group_messages::Column::DeletedAt.is_null())
		.order_by_desc(group_messages::Column::SentAt)
		.one(&state.database)
		.await?;

	let mut unread = false;
	let last_message = match last_message {
		Some(message) => {
			let sender = User::find_by_id(message.sender_id)
				.one(&state.database)
				.await?;

			if message.sender_id != viewer_id {
				let read = GroupMessageReads::find()
					.filter(group_message_reads::Column::GroupId.eq(group.id))
					.filter(group_message_reads::Column::UserId.eq(viewer_id))
					.one(&state.database)
					.await?;
				let last_read = read.and_then(|read| read.last_read_message_id);
				unread = last_read.is_none_or(|last_read| last_read < message.id);
			}

			sender.map(|sender| LastMessage {
				content: message.content,
				sender: sender.minecraft_uuid,
				sent_at: message.sent_at,
			})
		}
		None => None,
	};

	Ok(GroupSummary {
		id: group.id,
		kind: group.kind,
		name: group.name,
		members,
		last_message,
		unread,
	})
}

pub(crate) async fn get_or_create_dm_group(
	state: &ApiState,
	a: i32,
	b: i32,
) -> Result<(bool, entities::groups::Model), GroupError> {
	use entities::{group_members, groups, prelude::*};

	let a_dm_groups = GroupMembers::find()
		.filter(group_members::Column::UserId.eq(a))
		.inner_join(Groups)
		.filter(groups::Column::Kind.eq(GroupKind::Dm))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|member| member.group_id)
		.collect::<Vec<_>>();

	let existing = GroupMembers::find()
		.filter(group_members::Column::UserId.eq(b))
		.filter(group_members::Column::GroupId.is_in(a_dm_groups))
		.one(&state.database)
		.await?;

	if let Some(existing) = existing {
		return Ok((false, super::load_group(state, existing.group_id).await?));
	}

	let group = Groups::insert(groups::ActiveModel {
		kind: Set(GroupKind::Dm),
		name: Set(None),
		owner_id: Set(None),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	for member in [a, b] {
		GroupMembers::insert(group_members::ActiveModel {
			group_id: Set(group.id),
			user_id: Set(member),
			role: Set(GroupMemberRole::Member),
			joined_at: ActiveValue::NotSet,
		})
		.exec_without_returning(&state.database)
		.await?;
	}

	Ok((true, group))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn open_dm(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<(StatusCode, Json<GroupSummary>), GroupError> {
	let target = find_user_by_uuid(&state, target).await?;
	if target.id == player.id {
		return Err(GroupError::SelfTarget);
	}
	if is_blocked_either_way(&state, player.id, target.id).await? {
		return Err(GroupError::Blocked);
	}
	if !are_friends(&state, player.id, target.id).await? {
		return Err(GroupError::NotFriends);
	}

	let (created, group) = get_or_create_dm_group(&state, player.id, target.id).await?;

	Ok((
		if created { StatusCode::CREATED } else { StatusCode::OK },
		Json(summarize(&state, group, player.id).await?),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn claim(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<Json<GroupSummary>, GroupError> {
	require_membership(&state, id, player.id).await?;
	let group = load_group(&state, id).await?;
	if group.kind != GroupKind::Group || group.owner_id.is_some() || group.name.is_some() {
		return Err(GroupError::NotSpecialChatGroup);
	}

	let mut active: entities::groups::ActiveModel = group.into();
	active.owner_id = Set(Some(player.id));
	let group = active.update(&state.database).await?;

	Ok(Json(summarize(&state, group, player.id).await?))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn create_group(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<GroupSummary>), GroupError> {
	use entities::{group_members, groups, prelude::*};

	let mut member_ids = vec![player.id];
	for uuid in body.members {
		let user = find_user_by_uuid(&state, uuid).await?;
		if user.id == player.id {
			continue;
		}
		if is_blocked_either_way(&state, player.id, user.id).await? {
			return Err(GroupError::Blocked);
		}
		if !member_ids.contains(&user.id) {
			member_ids.push(user.id);
		}
	}

	if member_ids.len() > super::MAX_GROUP_MEMBERS {
		return Err(GroupError::GroupFull);
	}

	let group = Groups::insert(groups::ActiveModel {
		kind: Set(GroupKind::Group),
		name: Set(Some(body.name)),
		owner_id: Set(Some(player.id)),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	for member in member_ids {
		GroupMembers::insert(group_members::ActiveModel {
			group_id: Set(group.id),
			user_id: Set(member),
			role: Set(if member == player.id {
				GroupMemberRole::Owner
			} else {
				GroupMemberRole::Member
			}),
			joined_at: ActiveValue::NotSet,
		})
		.exec_without_returning(&state.database)
		.await?;
	}

	Ok((
		StatusCode::CREATED,
		Json(summarize(&state, group, player.id).await?),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<GroupSummary>>, GroupError> {
	use entities::{group_members, prelude::*};

	let group_ids = GroupMembers::find()
		.filter(group_members::Column::UserId.eq(player.id))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|member| member.group_id)
		.collect::<Vec<_>>();

	let groups = Groups::find()
		.filter(entities::groups::Column::Id.is_in(group_ids))
		.all(&state.database)
		.await?;

	let mut summaries = Vec::with_capacity(groups.len());
	for group in groups {
		summaries.push(summarize(&state, group, player.id).await?);
	}

	Ok(Json(summaries))
}

