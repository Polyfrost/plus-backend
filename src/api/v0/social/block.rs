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
use chrono::{DateTime, FixedOffset};
use entities::{
	blocks, prelude::*, relationship_requests, relationships,
	sea_orm_active_enums::RelationshipRequestStatus, user,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::OnConflict,
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		v0::{
			account::AuthenticatedPlayer,
			social::{SocialError, canonical_pair, find_user_by_uuid},
		},
	},
	utils::pagination::{
		Cursor, CursorPage, CursorQuery, Page, keyset_by_id, split_page,
	},
};

const V0_LIST_CAP: u64 = 200;

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlockedPlayer {
	player: Uuid,
	since: DateTime<FixedOffset>,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listBlockedPlayers")
		.summary("List blocked players")
		.description("Lists the authenticated player's newest 200 blocks.")
		.tag("social")
}

fn list_paged_doc(op: TransformOperation) -> TransformOperation {
	op.id("listBlockedPlayersPaged")
		.summary("List blocked players")
		.description("Newest first. Pass the returned `cursor` back for the next page.")
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
		.api_route(
			"/blocked/{player}",
			delete_with(self::unblock, self::unblock_doc),
		)
}

pub(super) fn v1_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/blocked", get_with(self::list_paged, self::list_paged_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<BlockedPlayer>>, SocialError> {
	Ok(Json(
		load(&state, player.id, Page::first(V0_LIST_CAP))
			.await?
			.items,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list_paged(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Query(query): Query<CursorQuery>,
) -> Result<Json<CursorPage<BlockedPlayer>>, SocialError> {
	Ok(Json(load(&state, player.id, query.into()).await?))
}

async fn load(
	state: &ApiState,
	player_id: i32,
	page: Page,
) -> Result<CursorPage<BlockedPlayer>, SocialError> {
	let rows = keyset_by_id(
		Blocks::find().filter(blocks::Column::BlockerId.eq(player_id)),
		blocks::Column::Id,
		page,
	)
	.all(&state.database)
	.await?;
	let (rows, cursor) = split_page(rows, page, |row| Cursor::id(row.id));

	let users: HashMap<_, _> = User::find()
		.filter(user::Column::Id.is_in(rows.iter().map(|row| row.blocked_id)))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect();

	let items = rows
		.into_iter()
		.filter_map(|row| {
			Some(BlockedPlayer {
				player: *users.get(&row.blocked_id)?,
				since: row.created_at,
			})
		})
		.collect();

	Ok(CursorPage { items, cursor })
}

#[tracing::instrument(level = "debug", skip(state))]
async fn block(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<StatusCode, SocialError> {
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
		.filter(
			relationship_requests::Column::Status.eq(RelationshipRequestStatus::Pending),
		)
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
	let target = find_user_by_uuid(&state, target).await?;

	Blocks::delete_many()
		.filter(blocks::Column::BlockerId.eq(player.id))
		.filter(blocks::Column::BlockedId.eq(target.id))
		.exec(&state.database)
		.await?;

	Ok(StatusCode::NO_CONTENT)
}
