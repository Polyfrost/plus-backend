use std::collections::HashMap;

use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, get_with},
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
	prelude::*,
	relationships,
	sea_orm_active_enums::{PlayerStatus, RelationshipKind},
	user,
};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Serialize;
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		state::effective_status,
		v0::{
			account::AuthenticatedPlayer,
			social::{SocialError, canonical_pair, find_user_by_uuid},
			websocket::{send_to_owner, structs::ClientBoundPacket},
		},
	},
	utils::pagination::{
		Cursor, CursorPage, CursorQuery, Page, keyset_by_id, split_page,
	},
};

const V0_LIST_CAP: u64 = 500;

#[derive(Debug, Serialize, JsonSchema)]
pub struct Friend {
	player: Uuid,
	kind: RelationshipKind,
	since: DateTime<FixedOffset>,
	/// Kept for older clients
	online: bool,
	status: PlayerStatus,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listFriends")
		.summary("List friends")
		.description("Lists the authenticated player's newest 500 friends.")
		.tag("social")
}

fn list_paged_doc(op: TransformOperation) -> TransformOperation {
	op.id("listFriendsPaged")
		.summary("List friends")
		.description("Newest first. Pass the returned `cursor` back for the next page.")
		.tag("social")
}

fn remove_doc(op: TransformOperation) -> TransformOperation {
	op.id("removeFriend")
		.summary("Remove a friend")
		.description("Ends a friend relationship with the given player, if one exists.")
		.tag("social")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/friends", get_with(self::list, self::list_doc))
		.api_route(
			"/friends/{player}",
			delete_with(self::remove, self::remove_doc),
		)
}

pub(super) fn v1_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/friends", get_with(self::list_paged, self::list_paged_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<Friend>>, SocialError> {
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
) -> Result<Json<CursorPage<Friend>>, SocialError> {
	Ok(Json(load(&state, player.id, query.into()).await?))
}

fn other_id(row: &relationships::Model, player_id: i32) -> i32 {
	if row.user_a_id == player_id {
		row.user_b_id
	} else {
		row.user_a_id
	}
}

async fn load(
	state: &ApiState,
	player_id: i32,
	page: Page,
) -> Result<CursorPage<Friend>, SocialError> {
	let rows = keyset_by_id(
		Relationships::find().filter(
			relationships::Column::UserAId
				.eq(player_id)
				.or(relationships::Column::UserBId.eq(player_id)),
		),
		relationships::Column::Id,
		page,
	)
	.all(&state.database)
	.await?;
	let (rows, cursor) = split_page(rows, page, |row| Cursor::id(row.id));

	let others: HashMap<_, _> = User::find()
		.filter(user::Column::Id.is_in(rows.iter().map(|row| other_id(row, player_id))))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, (user.minecraft_uuid, user.status_override)))
		.collect();

	let online = state.realtime.connections_by_owner.read().await;

	let items = rows
		.into_iter()
		.filter_map(|row| {
			let (uuid, status_override) = others.get(&other_id(&row, player_id))?;
			let status = effective_status(online.contains_key(uuid), *status_override);
			Some(Friend {
				player: *uuid,
				kind: row.kind,
				since: row.created_at,
				online: status != PlayerStatus::Offline,
				status,
			})
		})
		.collect();

	Ok(CursorPage { items, cursor })
}

#[tracing::instrument(level = "debug", skip(state))]
async fn remove(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<StatusCode, SocialError> {
	let target = find_user_by_uuid(&state, target).await?;
	let (low, high) = canonical_pair(player.id, target.id);

	let result = Relationships::delete_many()
		.filter(relationships::Column::UserAId.eq(low))
		.filter(relationships::Column::UserBId.eq(high))
		.exec(&state.database)
		.await?;

	if result.rows_affected == 0 {
		return Err(SocialError::NotFriends);
	}

	send_to_owner(&state, target.minecraft_uuid, || {
		ClientBoundPacket::FriendRemoved {
			player: player.minecraft_uuid,
		}
	})
	.await;

	Ok(StatusCode::NO_CONTENT)
}
