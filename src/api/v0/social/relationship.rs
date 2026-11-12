use aide::{
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
};
use entities::{
	blocks, prelude::*, relationship_requests,
	sea_orm_active_enums::RelationshipRequestStatus,
};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Select};
use serde::Serialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::{
		account::AuthenticatedPlayer,
		social::{SocialError, are_friends, find_user_by_uuid},
	},
};

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipStatus {
	Friend,
	/// Blocked by the authenticated player.
	Blocked,
	IncomingRequest,
	OutgoingRequest,
	None,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Relationship {
	relationship: RelationshipStatus,
	/// The pending request, for `incoming_request` and `outgoing_request`.
	request_id: Option<i32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RequestCounts {
	incoming: u64,
	outgoing: u64,
}

fn relationship_doc(op: TransformOperation) -> TransformOperation {
	op.id("getRelationship")
		.summary("Get the relationship with a player")
		.tag("social")
}

fn counts_doc(op: TransformOperation) -> TransformOperation {
	op.id("countFriendRequests")
		.summary("Count pending friend requests")
		.tag("social")
}

pub(super) fn v1_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/relationship/{player}",
			get_with(self::relationship, self::relationship_doc),
		)
		.api_route("/requests/counts", get_with(self::counts, self::counts_doc))
}

fn pending_request(sender: i32, recipient: i32) -> Select<RelationshipRequests> {
	RelationshipRequests::find()
		.filter(relationship_requests::Column::SenderId.eq(sender))
		.filter(relationship_requests::Column::RecipientId.eq(recipient))
		.filter(relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn relationship(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<Json<Relationship>, SocialError> {
	let target = find_user_by_uuid(&state, target).await?;
	if target.id == player.id {
		return Err(SocialError::SelfTarget);
	}

	let status = |relationship| Json(Relationship {
		relationship,
		request_id: None,
	});

	if Blocks::find()
		.filter(blocks::Column::BlockerId.eq(player.id))
		.filter(blocks::Column::BlockedId.eq(target.id))
		.one(&state.database)
		.await?
		.is_some()
	{
		return Ok(status(RelationshipStatus::Blocked));
	}
	if are_friends(&state, player.id, target.id).await? {
		return Ok(status(RelationshipStatus::Friend));
	}
	if let Some(request) = pending_request(target.id, player.id)
		.one(&state.database)
		.await?
	{
		return Ok(Json(Relationship {
			relationship: RelationshipStatus::IncomingRequest,
			request_id: Some(request.id),
		}));
	}
	if let Some(request) = pending_request(player.id, target.id)
		.one(&state.database)
		.await?
	{
		return Ok(Json(Relationship {
			relationship: RelationshipStatus::OutgoingRequest,
			request_id: Some(request.id),
		}));
	}

	Ok(status(RelationshipStatus::None))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn counts(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<RequestCounts>, SocialError> {
	let count = |column: relationship_requests::Column| {
		RelationshipRequests::find()
			.filter(column.eq(player.id))
			.filter(
				relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending),
			)
			.count(&state.database)
	};

	Ok(Json(RequestCounts {
		incoming: count(relationship_requests::Column::RecipientId).await?,
		outgoing: count(relationship_requests::Column::SenderId).await?,
	}))
}
