use std::collections::HashMap;

use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, Query, State},
	http::StatusCode,
};
use chrono::{DateTime, FixedOffset, Utc};
use entities::{
	prelude::*,
	relationship_requests, relationships,
	sea_orm_active_enums::{RelationshipKind, RelationshipRequestStatus},
	user,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue, ColumnTrait, DbErr, EntityTrait, QueryFilter, Set,
	sea_query::OnConflict,
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		v0::{
			account::AuthenticatedPlayer,
			social::{
				SocialError, canonical_pair, find_user_by_uuid, is_blocked_either_way,
			},
			websocket::{send_to_owner, structs::ClientBoundPacket},
		},
	},
	utils::pagination::{
		Cursor, CursorPage, CursorQuery, Page, keyset_by_id, split_page,
	},
};

const V0_LIST_CAP: u64 = 200;
const PAGED_DESCRIPTION: &str =
	"Newest first. Pass the returned `cursor` back for the next page.";

#[derive(Debug, Serialize, JsonSchema)]
pub struct FriendRequest {
	id: i32,
	player: Uuid,
	created_at: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, Copy)]
enum Direction {
	Incoming,
	Outgoing,
}

impl Direction {
	/// The column holding the listing player.
	fn own_column(self) -> relationship_requests::Column {
		match self {
			Self::Incoming => relationship_requests::Column::RecipientId,
			Self::Outgoing => relationship_requests::Column::SenderId,
		}
	}

	fn other_id(self, row: &relationship_requests::Model) -> i32 {
		match self {
			Self::Incoming => row.sender_id,
			Self::Outgoing => row.recipient_id,
		}
	}
}

fn incoming_doc(op: TransformOperation) -> TransformOperation {
	op.id("listIncomingFriendRequests")
		.summary("List incoming friend requests")
		.description(
			"Lists the newest 200 pending requests sent to the authenticated player.",
		)
		.tag("social")
}

fn outgoing_doc(op: TransformOperation) -> TransformOperation {
	op.id("listOutgoingFriendRequests")
		.summary("List outgoing friend requests")
		.description(
			"Lists the newest 200 pending requests the authenticated player sent.",
		)
		.tag("social")
}

fn incoming_paged_doc(op: TransformOperation) -> TransformOperation {
	op.id("listIncomingFriendRequestsPaged")
		.summary("List incoming friend requests")
		.description(PAGED_DESCRIPTION)
		.tag("social")
}

fn outgoing_paged_doc(op: TransformOperation) -> TransformOperation {
	op.id("listOutgoingFriendRequestsPaged")
		.summary("List outgoing friend requests")
		.description(PAGED_DESCRIPTION)
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
		.api_route(
			"/requests/incoming",
			get_with(self::incoming, self::incoming_doc),
		)
		.api_route(
			"/requests/outgoing",
			get_with(self::outgoing, self::outgoing_doc),
		)
		.api_route("/requests/{id}", post_with(self::send, self::send_doc))
		.api_route(
			"/requests/{id}/accept",
			post_with(self::accept, self::accept_doc),
		)
		.api_route(
			"/requests/{id}/decline",
			post_with(self::decline, self::decline_doc),
		)
		.api_route(
			"/requests/{id}",
			delete_with(self::cancel, self::cancel_doc),
		)
}

pub(super) fn v1_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/requests/incoming",
			get_with(self::incoming_paged, self::incoming_paged_doc),
		)
		.api_route(
			"/requests/outgoing",
			get_with(self::outgoing_paged, self::outgoing_paged_doc),
		)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn incoming(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<FriendRequest>>, SocialError> {
	let page = Page::first(V0_LIST_CAP);
	Ok(Json(
		load(&state, player.id, Direction::Incoming, page)
			.await?
			.items,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn outgoing(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<FriendRequest>>, SocialError> {
	let page = Page::first(V0_LIST_CAP);
	Ok(Json(
		load(&state, player.id, Direction::Outgoing, page)
			.await?
			.items,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn incoming_paged(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Query(query): Query<CursorQuery>,
) -> Result<Json<CursorPage<FriendRequest>>, SocialError> {
	Ok(Json(
		load(&state, player.id, Direction::Incoming, query.into()).await?,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn outgoing_paged(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Query(query): Query<CursorQuery>,
) -> Result<Json<CursorPage<FriendRequest>>, SocialError> {
	Ok(Json(
		load(&state, player.id, Direction::Outgoing, query.into()).await?,
	))
}

async fn load(
	state: &ApiState,
	player_id: i32,
	direction: Direction,
	page: Page,
) -> Result<CursorPage<FriendRequest>, SocialError> {
	let rows = keyset_by_id(
		RelationshipRequests::find()
			.filter(direction.own_column().eq(player_id))
			.filter(
				relationship_requests::Column::Status
					.eq(RelationshipRequestStatus::Pending),
			),
		relationship_requests::Column::Id,
		page,
	)
	.all(&state.database)
	.await?;
	let (rows, cursor) = split_page(rows, page, |row| Cursor::id(row.id));

	let users: HashMap<_, _> = User::find()
		.filter(user::Column::Id.is_in(rows.iter().map(|row| direction.other_id(row))))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect();

	let items = rows
		.into_iter()
		.filter_map(|row| {
			Some(FriendRequest {
				id: row.id,
				player: *users.get(&direction.other_id(&row))?,
				created_at: row.created_at,
			})
		})
		.collect();

	Ok(CursorPage { items, cursor })
}

async fn make_friends(state: &ApiState, a: i32, b: i32) -> Result<(), DbErr> {
	let (low, high) = canonical_pair(a, b);

	Relationships::insert(relationships::ActiveModel {
		user_a_id: Set(low),
		user_b_id: Set(high),
		kind: Set(RelationshipKind::Friend),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.on_conflict(
		OnConflict::columns([
			relationships::Column::UserAId,
			relationships::Column::UserBId,
		])
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
	let target = find_user_by_uuid(&state, target).await?;
	if target.id == player.id {
		return Err(SocialError::SelfTarget);
	}
	if is_blocked_either_way(&state, player.id, target.id).await? {
		return Err(SocialError::Blocked);
	}
	if let Some(wait) = state.friend_request_limit.check(player.id).await {
		return Err(SocialError::RateLimited(wait));
	}

	let (low, high) = canonical_pair(player.id, target.id);
	let already_friends = Relationships::find()
		.filter(relationships::Column::UserAId.eq(low))
		.filter(relationships::Column::UserBId.eq(high))
		.one(&state.database)
		.await?
		.is_some();
	if already_friends {
		return Err(SocialError::AlreadyFriends);
	}

	if let Some(incoming) = RelationshipRequests::find()
		.filter(relationship_requests::Column::SenderId.eq(target.id))
		.filter(relationship_requests::Column::RecipientId.eq(player.id))
		.filter(
			relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending),
		)
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
		.filter(
			relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending),
		)
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
) -> Result<relationship_requests::Model, SocialError> {
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
	let request = load_pending_request(&state, id).await?;
	if request.recipient_id != player.id {
		return Err(SocialError::RequestForbidden);
	}

	let mut active: relationship_requests::ActiveModel = request.clone().into();
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
	let request = load_pending_request(&state, id).await?;
	if request.recipient_id != player.id {
		return Err(SocialError::RequestForbidden);
	}

	let mut active: relationship_requests::ActiveModel = request.clone().into();
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

	let mut active: relationship_requests::ActiveModel = request.into();
	active.status = Set(RelationshipRequestStatus::Cancelled);
	active.responded_at = Set(Some(Utc::now().into()));
	active.update(&state.database).await?;

	Ok(StatusCode::NO_CONTENT)
}
