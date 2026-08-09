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
use chrono::{DateTime, FixedOffset, Utc};
use entities::sea_orm_active_enums::{RelationshipKind, RelationshipRequestStatus};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Serialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::account::AuthenticatedPlayer,
	v0::social::{SocialError, canonical_pair, find_user_by_uuid, is_blocked_either_way},
	v0::websocket::{send_to_owner, structs::ClientBoundPacket},
};

#[derive(Debug, Serialize, JsonSchema)]
pub struct FriendRequest {
	id: i32,
	player: Uuid,
	created_at: DateTime<FixedOffset>,
}

fn incoming_doc(op: TransformOperation) -> TransformOperation {
	op.id("listIncomingFriendRequests")
		.summary("List incoming friend requests")
		.tag("social")
}

fn outgoing_doc(op: TransformOperation) -> TransformOperation {
	op.id("listOutgoingFriendRequests")
		.summary("List outgoing friend requests")
		.tag("social")
}

fn send_doc(op: TransformOperation) -> TransformOperation {
	op.id("sendFriendRequest")
		.summary("Send a friend request")
		.description(
			"Sends a friend request to the given player. If that player has \
			 already sent the authenticated player a request, the two are \
			 immediately made friends instead.",
		)
		.tag("social")
}

fn accept_doc(op: TransformOperation) -> TransformOperation {
	op.id("acceptFriendRequest")
		.summary("Accept a friend request")
		.tag("social")
}

fn decline_doc(op: TransformOperation) -> TransformOperation {
	op.id("declineFriendRequest")
		.summary("Decline a friend request")
		.tag("social")
}

fn cancel_doc(op: TransformOperation) -> TransformOperation {
	op.id("cancelFriendRequest")
		.summary("Cancel a sent friend request")
		.tag("social")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/requests/incoming", get_with(self::incoming, self::incoming_doc))
		.api_route("/requests/outgoing", get_with(self::outgoing, self::outgoing_doc))
		.api_route("/requests/{id}", post_with(self::send, self::send_doc))
		.api_route("/requests/{id}/accept", post_with(self::accept, self::accept_doc))
		.api_route("/requests/{id}/decline", post_with(self::decline, self::decline_doc))
		.api_route("/requests/{id}", delete_with(self::cancel, self::cancel_doc))
}

async fn requests_with_uuid(
	state: &ApiState,
	rows: Vec<entities::relationship_requests::Model>,
	uuid_of: impl Fn(&entities::relationship_requests::Model) -> i32,
) -> Result<Vec<FriendRequest>, SocialError> {
	use entities::{prelude::*, user};

	let ids = rows.iter().map(&uuid_of).collect::<Vec<_>>();
	let users = User::find()
		.filter(user::Column::Id.is_in(ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect::<std::collections::HashMap<_, _>>();

	Ok(rows
		.into_iter()
		.filter_map(|row| {
			let other = uuid_of(&row);
			users.get(&other).map(|uuid| FriendRequest {
				id: row.id,
				player: *uuid,
				created_at: row.created_at,
			})
		})
		.collect())
}

#[tracing::instrument(level = "debug", skip(state))]
async fn incoming(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<FriendRequest>>, SocialError> {
	use entities::{prelude::*, relationship_requests};

	let rows = RelationshipRequests::find()
		.filter(relationship_requests::Column::RecipientId.eq(player.id))
		.filter(relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending))
		.all(&state.database)
		.await?;

	Ok(Json(
		requests_with_uuid(&state, rows, |row| row.sender_id).await?,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn outgoing(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<FriendRequest>>, SocialError> {
	use entities::{prelude::*, relationship_requests};

	let rows = RelationshipRequests::find()
		.filter(relationship_requests::Column::SenderId.eq(player.id))
		.filter(relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending))
		.all(&state.database)
		.await?;

	Ok(Json(
		requests_with_uuid(&state, rows, |row| row.recipient_id).await?,
	))
}

async fn make_friends(state: &ApiState, a: i32, b: i32) -> Result<(), sea_orm::DbErr> {
	use entities::{prelude::*, relationships};
	use sea_orm::sea_query::OnConflict;

	let (low, high) = canonical_pair(a, b);

	Relationships::insert(relationships::ActiveModel {
		user_a_id: Set(low),
		user_b_id: Set(high),
		kind: Set(RelationshipKind::Friend),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.on_conflict(
		OnConflict::columns([relationships::Column::UserAId, relationships::Column::UserBId])
			.do_nothing()
			.to_owned(),
	)
	.exec_without_returning(&state.database)
	.await?;

	Ok(())
}

#[tracing::instrument(level = "debug", skip(state))]
async fn send(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<(StatusCode, Json<Option<FriendRequest>>), SocialError> {
	use entities::{prelude::*, relationship_requests};

	let target = find_user_by_uuid(&state, target).await?;
	if target.id == player.id {
		return Err(SocialError::SelfTarget);
	}
	if is_blocked_either_way(&state, player.id, target.id).await? {
		return Err(SocialError::Blocked);
	}

	let (low, high) = canonical_pair(player.id, target.id);
	let already_friends = Relationships::find()
		.filter(entities::relationships::Column::UserAId.eq(low))
		.filter(entities::relationships::Column::UserBId.eq(high))
		.one(&state.database)
		.await?
		.is_some();
	if already_friends {
		return Err(SocialError::AlreadyFriends);
	}

	if let Some(incoming) = RelationshipRequests::find()
		.filter(relationship_requests::Column::SenderId.eq(target.id))
		.filter(relationship_requests::Column::RecipientId.eq(player.id))
		.filter(relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending))
		.one(&state.database)
		.await?
	{
		let mut active: relationship_requests::ActiveModel = incoming.clone().into();
		active.status = Set(RelationshipRequestStatus::Accepted);
		active.responded_at = Set(Some(Utc::now().into()));
		active.update(&state.database).await?;

		make_friends(&state, player.id, target.id).await?;

		send_to_owner(&state, target.minecraft_uuid, || {
			ClientBoundPacket::FriendRequestUpdated {
				request_id: incoming.id,
				status: "accepted".to_string(),
			}
		})
		.await;

		return Ok((StatusCode::OK, Json(None)));
	}

	let existing_outgoing = RelationshipRequests::find()
		.filter(relationship_requests::Column::SenderId.eq(player.id))
		.filter(relationship_requests::Column::RecipientId.eq(target.id))
		.filter(relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending))
		.one(&state.database)
		.await?;
	if existing_outgoing.is_some() {
		return Err(SocialError::RequestAlreadyPending);
	}

	let request = RelationshipRequests::insert(relationship_requests::ActiveModel {
		sender_id: Set(player.id),
		recipient_id: Set(target.id),
		status: Set(RelationshipRequestStatus::Pending),
		created_at: ActiveValue::NotSet,
		responded_at: Set(None),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	send_to_owner(&state, target.minecraft_uuid, || {
		ClientBoundPacket::FriendRequestReceived {
			request_id: request.id,
			sender: player.minecraft_uuid,
		}
	})
	.await;

	Ok((
		StatusCode::CREATED,
		Json(Some(FriendRequest {
			id: request.id,
			player: target.minecraft_uuid,
			created_at: request.created_at,
		})),
	))
}

async fn load_pending_request(
	state: &ApiState,
	id: i32,
) -> Result<entities::relationship_requests::Model, SocialError> {
	use entities::prelude::*;

	RelationshipRequests::find_by_id(id)
		.one(&state.database)
		.await?
		.filter(|request| request.status == RelationshipRequestStatus::Pending)
		.ok_or(SocialError::RequestMissing)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn accept(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, SocialError> {
	use entities::prelude::*;

	let request = load_pending_request(&state, id).await?;
	if request.recipient_id != player.id {
		return Err(SocialError::RequestForbidden);
	}

	let mut active: entities::relationship_requests::ActiveModel = request.clone().into();
	active.status = Set(RelationshipRequestStatus::Accepted);
	active.responded_at = Set(Some(Utc::now().into()));
	active.update(&state.database).await?;

	make_friends(&state, player.id, request.sender_id).await?;

	if let Some(sender) = User::find_by_id(request.sender_id)
		.one(&state.database)
		.await?
	{
		send_to_owner(&state, sender.minecraft_uuid, || {
			ClientBoundPacket::FriendRequestUpdated {
				request_id: request.id,
				status: "accepted".to_string(),
			}
		})
		.await;
	}

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn decline(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, SocialError> {
	use entities::prelude::*;

	let request = load_pending_request(&state, id).await?;
	if request.recipient_id != player.id {
		return Err(SocialError::RequestForbidden);
	}

	let mut active: entities::relationship_requests::ActiveModel = request.clone().into();
	active.status = Set(RelationshipRequestStatus::Declined);
	active.responded_at = Set(Some(Utc::now().into()));
	active.update(&state.database).await?;

	if let Some(sender) = User::find_by_id(request.sender_id)
		.one(&state.database)
		.await?
	{
		send_to_owner(&state, sender.minecraft_uuid, || {
			ClientBoundPacket::FriendRequestUpdated {
				request_id: request.id,
				status: "declined".to_string(),
			}
		})
		.await;
	}

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn cancel(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, SocialError> {
	let request = load_pending_request(&state, id).await?;
	if request.sender_id != player.id {
		return Err(SocialError::RequestForbidden);
	}

	let mut active: entities::relationship_requests::ActiveModel = request.into();
	active.status = Set(RelationshipRequestStatus::Cancelled);
	active.responded_at = Set(Some(Utc::now().into()));
	active.update(&state.database).await?;

	Ok(StatusCode::NO_CONTENT)
}
