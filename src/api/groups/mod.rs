mod create;
mod members;
mod messages;

use aide::{OperationIo, axum::ApiRouter};
use axum::{http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::GroupKind;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::api::ApiState;

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

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/groups",
		ApiRouter::new()
			.merge(create::router())
			.merge(members::router())
			.merge(messages::router()),
	)
}
