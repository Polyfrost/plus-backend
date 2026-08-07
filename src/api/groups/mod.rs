mod create;
mod members;
mod messages;

use aide::{OperationIo, axum::ApiRouter};
use axum::{http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::GroupKind;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::api::ApiState;

pub(crate) use create::get_or_create_dm_group;

pub(super) const MAX_DM_MEMBERS: usize = 2;
pub(super) const MAX_GROUP_MEMBERS: usize = 50;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum GroupError {
	#[error("The requested player does not exist")]
	PlayerMissing,
	#[error("You cannot perform this action on yourself")]
	SelfTarget,
	#[error("That player is blocked, or has blocked you")]
	Blocked,
	#[error("You must be friends with a player to start a direct message")]
	NotFriends,
	#[error("No such group")]
	GroupMissing,
	#[error("You are not a member of that group")]
	NotAMember,
	#[error("Only the group owner may perform this action")]
	NotOwner,
	#[error("Direct messages cannot have members added or removed")]
	DmImmutable,
	#[error("That group is full")]
	GroupFull,
	#[error("That player is already a member of this group")]
	AlreadyMember,
	#[error("No such message")]
	MessageMissing,
	#[error("Message content must be between 1 and 4000 characters")]
	InvalidContent,
	#[error("You may only edit or delete your own messages")]
	MessageForbidden,
	#[error(
		"You may only send one message to a given Special Chat recipient every \
		 72 hours"
	)]
	RateLimited,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for GroupError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::PlayerMissing | Self::GroupMissing | Self::MessageMissing => {
					StatusCode::NOT_FOUND
				}
				Self::SelfTarget
				| Self::Blocked
				| Self::NotFriends
				| Self::DmImmutable
				| Self::GroupFull
				| Self::AlreadyMember => StatusCode::CONFLICT,
				Self::InvalidContent => StatusCode::BAD_REQUEST,
				Self::NotAMember | Self::NotOwner | Self::MessageForbidden => {
					StatusCode::FORBIDDEN
				}
				Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

pub(super) async fn find_user_by_uuid(
	state: &ApiState,
	uuid: uuid::Uuid,
) -> Result<entities::user::Model, GroupError> {
	use entities::{prelude::*, user};

	User::find()
		.filter(user::Column::MinecraftUuid.eq(uuid))
		.one(&state.database)
		.await?
		.ok_or(GroupError::PlayerMissing)
}

pub(super) async fn load_group(
	state: &ApiState,
	group_id: i32,
) -> Result<entities::groups::Model, GroupError> {
	use entities::prelude::*;

	Groups::find_by_id(group_id)
		.one(&state.database)
		.await?
		.ok_or(GroupError::GroupMissing)
}

pub(super) async fn require_membership(
	state: &ApiState,
	group_id: i32,
	user_id: i32,
) -> Result<entities::group_members::Model, GroupError> {
	use entities::{group_members, prelude::*};

	GroupMembers::find()
		.filter(group_members::Column::GroupId.eq(group_id))
		.filter(group_members::Column::UserId.eq(user_id))
		.one(&state.database)
		.await?
		.ok_or(GroupError::NotAMember)
}

pub(super) async fn member_ids(
	state: &ApiState,
	group_id: i32,
) -> Result<Vec<i32>, sea_orm::DbErr> {
	use entities::{group_members, prelude::*};

	Ok(GroupMembers::find()
		.filter(group_members::Column::GroupId.eq(group_id))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|member| member.user_id)
		.collect())
}

pub(super) fn member_cap(kind: &GroupKind) -> usize {
	match kind {
		GroupKind::Dm => MAX_DM_MEMBERS,
		GroupKind::Group => MAX_GROUP_MEMBERS,
	}
}

pub(super) async fn enforce_special_chat_cooldown(
	state: &ApiState,
	group: &entities::groups::Model,
	sender_id: i32,
) -> Result<(), GroupError> {
	use entities::prelude::*;

	if group.kind != GroupKind::Dm {
		return Ok(());
	}

	let Some(other_id) = member_ids(state, group.id)
		.await?
		.into_iter()
		.find(|id| *id != sender_id)
	else {
		return Ok(());
	};

	let Some(other_user) = User::find_by_id(other_id).one(&state.database).await? else {
		return Ok(());
	};

	if !state.special_chat_targets.contains(&other_user.minecraft_uuid) {
		return Ok(());
	}

	let cooldown_key = (sender_id, other_user.id);
	if state.special_chat_cooldown.get(&cooldown_key).await.is_some() {
		return Err(GroupError::RateLimited);
	}
	state.special_chat_cooldown.insert(cooldown_key, ()).await;

	Ok(())
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/groups",
		ApiRouter::new()
			.merge(create::router())
			.merge(members::router())
			.merge(messages::router()),
	)
}
