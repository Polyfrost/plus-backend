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
use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Serialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::account::AuthenticatedPlayer,
	v0::social::{SocialError, canonical_pair, find_user_by_uuid},
};

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlockedPlayer {
	player: Uuid,
	since: DateTime<FixedOffset>,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listBlockedPlayers")
		.summary("List blocked players")
		.tag("social")
}

fn block_doc(op: TransformOperation) -> TransformOperation {
	op.id("blockPlayer")
		.summary("Block a player")
		.description(
			"Blocks a player, ending any existing friendship and pending \
			 requests between the two. Blocked players cannot send friend \
			 requests, messages, or invites to the blocking player.",
		)
		.tag("social")
}

fn unblock_doc(op: TransformOperation) -> TransformOperation {
	op.id("unblockPlayer")
		.summary("Unblock a player")
		.tag("social")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/blocked", get_with(self::list, self::list_doc))
		.api_route("/blocked/{player}", post_with(self::block, self::block_doc))
		.api_route("/blocked/{player}", delete_with(self::unblock, self::unblock_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<BlockedPlayer>>, SocialError> {
	use entities::{blocks, prelude::*, user};

	let rows = Blocks::find()
		.filter(blocks::Column::BlockerId.eq(player.id))
		.all(&state.database)
		.await?;

	let ids = rows.iter().map(|row| row.blocked_id).collect::<Vec<_>>();
	let users = User::find()
		.filter(user::Column::Id.is_in(ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect::<std::collections::HashMap<_, _>>();

	Ok(Json(
		rows.into_iter()
			.filter_map(|row| {
				users.get(&row.blocked_id).map(|uuid| BlockedPlayer {
					player: *uuid,
					since: row.created_at,
				})
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn block(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<StatusCode, SocialError> {
	use entities::{blocks, prelude::*, relationship_requests, relationships};
	use sea_orm::sea_query::OnConflict;

	let target = find_user_by_uuid(&state, target).await?;
	if target.id == player.id {
		return Err(SocialError::SelfTarget);
	}

	Blocks::insert(blocks::ActiveModel {
		blocker_id: Set(player.id),
		blocked_id: Set(target.id),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.on_conflict(
		OnConflict::columns([blocks::Column::BlockerId, blocks::Column::BlockedId])
			.do_nothing()
			.to_owned(),
	)
	.exec_without_returning(&state.database)
	.await?;

	let (low, high) = canonical_pair(player.id, target.id);
	Relationships::delete_many()
		.filter(relationships::Column::UserAId.eq(low))
		.filter(relationships::Column::UserBId.eq(high))
		.exec(&state.database)
		.await?;

	RelationshipRequests::delete_many()
		.filter(
			relationship_requests::Column::SenderId
				.eq(player.id)
				.and(relationship_requests::Column::RecipientId.eq(target.id))
				.or(relationship_requests::Column::SenderId
					.eq(target.id)
					.and(relationship_requests::Column::RecipientId.eq(player.id))),
		)
		.filter(relationship_requests::Column::Status.eq(entities::sea_orm_active_enums::RelationshipRequestStatus::Pending))
		.exec(&state.database)
		.await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn unblock(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<StatusCode, SocialError> {
	use entities::{blocks, prelude::*};

	let target = find_user_by_uuid(&state, target).await?;

	Blocks::delete_many()
		.filter(blocks::Column::BlockerId.eq(player.id))
		.filter(blocks::Column::BlockedId.eq(target.id))
		.exec(&state.database)
		.await?;

	Ok(StatusCode::NO_CONTENT)
}
