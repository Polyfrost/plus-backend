use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, patch_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use entities::{
	groups,
	sea_orm_active_enums::{GroupKind, GroupMemberRole},
};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::groups::{GroupError, OwnPlayer, find_user_by_uuid, load_group, member_cap, member_ids, messages::notify_other_members, require_membership},
	v0::social::is_blocked_either_way,
	v0::websocket::structs::ClientBoundPacket,
};

const MAX_GROUP_NAME_LENGTH: usize = 64;

#[derive(Debug, Deserialize, JsonSchema)]
struct RenameGroupRequest {
	name: String,
}

fn rename_doc(op: TransformOperation) -> TransformOperation {
	op.id("renameGroup")
		.summary("Rename a group chat")
		.description(
			"Any member may rename the group. Not applicable to DMs or the \
			 Special Chat group. Other members receive a `GroupRenamed` event.",
		)
		.tag("groups")
}

fn add_doc(op: TransformOperation) -> TransformOperation {
	op.id("addGroupMember")
		.summary("Add a member to a group chat")
		.description("Only the group owner may add members. Not applicable to DMs.")
		.tag("groups")
}

fn remove_doc(op: TransformOperation) -> TransformOperation {
	op.id("removeGroupMember")
		.summary("Remove a member from a group chat, or leave it")
		.description(
			"The group owner may remove any member; any member may remove \
			 themself (leave). Not applicable to DMs.",
		)
		.tag("groups")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/{id}/members/{player}", post_with(self::add, self::add_doc))
		.api_route(
			"/{id}/members/{player}",
			delete_with(self::remove, self::remove_doc),
		)
}

pub(super) fn v1_router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/{id}", patch_with(self::rename, self::rename_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn rename(
	State(state): State<ApiState>,
	OwnPlayer(player): OwnPlayer,
	Path(id): Path<i32>,
	Json(body): Json<RenameGroupRequest>,
) -> Result<StatusCode, GroupError> {
	let name = body.name.trim();
	if name.is_empty() || name.chars().count() > MAX_GROUP_NAME_LENGTH {
		return Err(GroupError::InvalidName);
	}

	require_membership(&state, id, player.id).await?;
	let group = load_group(&state, id).await?;
	if group.kind == GroupKind::Dm {
		return Err(GroupError::DmImmutable);
	}
	if group.kind == GroupKind::Group && group.owner_id.is_none() && group.name.is_none() {
		return Err(GroupError::SpecialChatImmutable);
	}

	let mut active: groups::ActiveModel = group.into();
	active.name = Set(Some(name.to_string()));
	active.update(&state.database).await?;

	notify_other_members(&state, id, player.id, || ClientBoundPacket::GroupRenamed {
		group_id: id,
		name: name.to_string(),
	})
	.await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn add(
	State(state): State<ApiState>,
	OwnPlayer(player): OwnPlayer,
	Path((id, target)): Path<(i32, Uuid)>,
) -> Result<StatusCode, GroupError> {
	use entities::{group_members, prelude::*};

	let group = load_group(&state, id).await?;
	if group.kind == GroupKind::Dm {
		return Err(GroupError::DmImmutable);
	}
	if group.kind == GroupKind::Party {
		return Err(GroupError::PartyManaged);
	}
	if group.owner_id != Some(player.id) {
		return Err(GroupError::NotOwner);
	}

	let target = find_user_by_uuid(&state, target).await?;
	if is_blocked_either_way(&state, player.id, target.id).await? {
		return Err(GroupError::Blocked);
	}

	let existing_members = member_ids(&state, id).await?;
	if existing_members.contains(&target.id) {
		return Err(GroupError::AlreadyMember);
	}
	if existing_members.len() >= member_cap(&group.kind) {
		return Err(GroupError::GroupFull);
	}

	GroupMembers::insert(group_members::ActiveModel {
		group_id: Set(id),
		user_id: Set(target.id),
		role: Set(GroupMemberRole::Member),
		joined_at: ActiveValue::NotSet,
	})
	.exec_without_returning(&state.database)
	.await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn remove(
	State(state): State<ApiState>,
	OwnPlayer(player): OwnPlayer,
	Path((id, target)): Path<(i32, Uuid)>,
) -> Result<StatusCode, GroupError> {
	use entities::{group_members, prelude::*};

	let group = load_group(&state, id).await?;
	if group.kind == GroupKind::Dm {
		return Err(GroupError::DmImmutable);
	}
	if group.kind == GroupKind::Party {
		return Err(GroupError::PartyManaged);
	}
	if group.owner_id.is_none() && group.name.is_none() {
		return Err(GroupError::SpecialChatImmutable);
	}

	let target = find_user_by_uuid(&state, target).await?;
	let is_self = target.id == player.id;
	if !is_self && group.owner_id != Some(player.id) {
		return Err(GroupError::NotOwner);
	}
	if !is_self {
		require_membership(&state, id, player.id).await?;
	}

	let result = GroupMembers::delete_many()
		.filter(group_members::Column::GroupId.eq(id))
		.filter(group_members::Column::UserId.eq(target.id))
		.exec(&state.database)
		.await?;

	if result.rows_affected == 0 {
		return Err(GroupError::NotAMember);
	}

	Ok(StatusCode::NO_CONTENT)
}
