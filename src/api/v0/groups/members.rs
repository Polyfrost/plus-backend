use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	extract::{Path, State},
	http::StatusCode,
};
use entities::sea_orm_active_enums::{GroupKind, GroupMemberRole};
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::account::AuthenticatedPlayer,
	v0::groups::{GroupError, find_user_by_uuid, load_group, member_cap, member_ids, require_membership},
	v0::social::is_blocked_either_way,
};

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

#[tracing::instrument(level = "debug", skip(state))]
async fn add(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path((id, target)): Path<(i32, Uuid)>,
) -> Result<StatusCode, GroupError> {
	use entities::{group_members, prelude::*};

	let group = load_group(&state, id).await?;
	if group.kind == GroupKind::Dm {
		return Err(GroupError::DmImmutable);
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
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path((id, target)): Path<(i32, Uuid)>,
) -> Result<StatusCode, GroupError> {
	use entities::{group_members, prelude::*};

	let group = load_group(&state, id).await?;
	if group.kind == GroupKind::Dm {
		return Err(GroupError::DmImmutable);
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
