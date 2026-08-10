mod block;
mod friends;
mod requests;

use aide::{OperationIo, axum::ApiRouter};
use axum::{http::StatusCode, response::IntoResponse};

use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SocialError {
	#[error("The requested player does not exist")]
	PlayerMissing,
	#[error("You cannot perform this action on yourself")]
	SelfTarget,
	#[error("You are not friends with that player")]
	NotFriends,
	#[error("You are already friends with that player")]
	AlreadyFriends,
	#[error("That player is blocked, or has blocked you")]
	Blocked,
	#[error("A pending friend request already exists between you and that player")]
	RequestAlreadyPending,
	#[error("No such friend request")]
	RequestMissing,
	#[error("You do not have permission to act on that request")]
	RequestForbidden,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for SocialError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::PlayerMissing | Self::RequestMissing => StatusCode::NOT_FOUND,
				Self::SelfTarget
				| Self::NotFriends
				| Self::AlreadyFriends
				| Self::Blocked
				| Self::RequestAlreadyPending => StatusCode::CONFLICT,
				Self::RequestForbidden => StatusCode::FORBIDDEN,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

pub(super) fn canonical_pair(a: i32, b: i32) -> (i32, i32) {
	if a <= b { (a, b) } else { (b, a) }
}

pub(super) async fn find_user_by_uuid(
	state: &ApiState,
	uuid: uuid::Uuid,
) -> Result<entities::user::Model, SocialError> {
	use entities::{prelude::*, user};
	use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

	User::find()
		.filter(user::Column::MinecraftUuid.eq(uuid))
		.one(&state.database)
		.await?
		.ok_or(SocialError::PlayerMissing)
}

pub(crate) async fn are_friends(
	state: &ApiState,
	a: i32,
	b: i32,
) -> Result<bool, sea_orm::DbErr> {
	use entities::{prelude::*, relationships};
	use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

	let (low, high) = canonical_pair(a, b);
	Ok(Relationships::find()
		.filter(relationships::Column::UserAId.eq(low))
		.filter(relationships::Column::UserBId.eq(high))
		.one(&state.database)
		.await?
		.is_some())
}

pub(super) async fn is_blocked_either_way(
	state: &ApiState,
	a: i32,
	b: i32,
) -> Result<bool, sea_orm::DbErr> {
	use entities::{blocks, prelude::*};
	use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

	Ok(Blocks::find()
		.filter(
			blocks::Column::BlockerId
				.eq(a)
				.and(blocks::Column::BlockedId.eq(b))
				.or(blocks::Column::BlockerId
					.eq(b)
					.and(blocks::Column::BlockedId.eq(a))),
		)
		.one(&state.database)
		.await?
		.is_some())
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/social",
		ApiRouter::new()
			.merge(friends::router())
			.merge(requests::router())
			.merge(block::router()),
	)
}
