mod block;
mod friends;
mod relationship;
mod requests;

use std::{
	collections::{HashMap, HashSet},
	time::Duration,
};

use aide::{OperationIo, axum::ApiRouter};
use axum::{
	http::{HeaderValue, StatusCode, header},
	response::{IntoResponse, Response},
};
use entities::{blocks, prelude::*, relationships, user};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect};
use uuid::Uuid;

use crate::api::{ApiState, error_response};

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
	#[error("You are sending friend requests too quickly")]
	RateLimited(Duration),
	#[error("Unable to query database: {0}")]
	Database(#[from] DbErr),
}

impl SocialError {
	fn status(&self) -> StatusCode {
		match self {
			Self::PlayerMissing | Self::RequestMissing => StatusCode::NOT_FOUND,
			Self::SelfTarget
			| Self::NotFriends
			| Self::AlreadyFriends
			| Self::Blocked
			| Self::RequestAlreadyPending => StatusCode::CONFLICT,
			Self::RequestForbidden => StatusCode::FORBIDDEN,
			Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
			Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
		}
	}
}

impl IntoResponse for SocialError {
	fn into_response(self) -> Response {
		let retry_after = match self {
			Self::RateLimited(wait) => {
				HeaderValue::from_str(&wait.as_secs().max(1).to_string()).ok()
			}
			_ => None,
		};
		let mut response = error_response(self.status(), self);
		if let Some(retry_after) = retry_after {
			response
				.headers_mut()
				.insert(header::RETRY_AFTER, retry_after);
		}
		response
	}
}

pub(super) fn canonical_pair(a: i32, b: i32) -> (i32, i32) {
	if a <= b { (a, b) } else { (b, a) }
}

pub(super) async fn find_user_by_uuid(
	state: &ApiState,
	uuid: Uuid,
) -> Result<user::Model, SocialError> {
	User::find()
		.filter(user::Column::MinecraftUuid.eq(uuid))
		.one(&state.database)
		.await?
		.ok_or(SocialError::PlayerMissing)
}

pub(crate) async fn are_friends(state: &ApiState, a: i32, b: i32) -> Result<bool, DbErr> {
	let (low, high) = canonical_pair(a, b);
	Ok(Relationships::find()
		.filter(relationships::Column::UserAId.eq(low))
		.filter(relationships::Column::UserBId.eq(high))
		.one(&state.database)
		.await?
		.is_some())
}

async fn user_ids(
	db: &impl ConnectionTrait,
	players: impl IntoIterator<Item = Uuid>,
) -> Result<HashMap<i32, Uuid>, DbErr> {
	let players = players.into_iter().collect::<HashSet<_>>();
	if players.is_empty() {
		return Ok(HashMap::new());
	}
	Ok(User::find()
		.select_only()
		.column(user::Column::Id)
		.column(user::Column::MinecraftUuid)
		.filter(user::Column::MinecraftUuid.is_in(players))
		.into_tuple::<(i32, Uuid)>()
		.all(db)
		.await?
		.into_iter()
		.collect())
}

pub(crate) async fn friend_pairs_among(
	db: &impl ConnectionTrait,
	focus: &[Uuid],
	others: &[Uuid],
) -> Result<Vec<(Uuid, Uuid)>, DbErr> {
	let focus_set = focus.iter().copied().collect::<HashSet<_>>();
	let ids = user_ids(db, focus.iter().chain(others).copied()).await?;
	let focus_ids = ids
		.iter()
		.filter(|(_, uuid)| focus_set.contains(uuid))
		.map(|(id, _)| *id)
		.collect::<Vec<_>>();
	if focus_ids.is_empty() || ids.len() < 2 {
		return Ok(Vec::new());
	}
	let all_ids = ids.keys().copied().collect::<Vec<_>>();
	Ok(Relationships::find()
		.filter(relationships::Column::UserAId.is_in(all_ids.clone()))
		.filter(relationships::Column::UserBId.is_in(all_ids))
		.filter(
			relationships::Column::UserAId
				.is_in(focus_ids.clone())
				.or(relationships::Column::UserBId.is_in(focus_ids)),
		)
		.all(db)
		.await?
		.into_iter()
		.filter_map(|row| Some((*ids.get(&row.user_a_id)?, *ids.get(&row.user_b_id)?)))
		.collect())
}

pub(crate) async fn friends_of(
	db: &impl ConnectionTrait,
	player: Uuid,
) -> Result<HashSet<Uuid>, DbErr> {
	let Some(player_id) = user_ids(db, [player]).await?.into_keys().next() else {
		return Ok(HashSet::new());
	};
	let others = Relationships::find()
		.filter(
			relationships::Column::UserAId
				.eq(player_id)
				.or(relationships::Column::UserBId.eq(player_id)),
		)
		.all(db)
		.await?
		.into_iter()
		.map(|row| {
			if row.user_a_id == player_id {
				row.user_b_id
			} else {
				row.user_a_id
			}
		})
		.collect::<HashSet<_>>();
	if others.is_empty() {
		return Ok(HashSet::new());
	}
	Ok(User::find()
		.select_only()
		.column(user::Column::MinecraftUuid)
		.filter(user::Column::Id.is_in(others))
		.into_tuple::<Uuid>()
		.all(db)
		.await?
		.into_iter()
		.collect())
}

pub(crate) async fn is_blocked_either_way(
	state: &ApiState,
	a: i32,
	b: i32,
) -> Result<bool, DbErr> {
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

/// The paged lists, served under `/v1/social`.
pub(crate) fn v1_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(friends::v1_router())
		.merge(requests::v1_router())
		.merge(block::v1_router())
		.merge(relationship::v1_router())
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
