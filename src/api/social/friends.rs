use aide::{
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, FixedOffset};
use entities::sea_orm_active_enums::RelationshipKind;
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Serialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	account::AuthenticatedPlayer,
	social::{SocialError, canonical_pair, find_user_by_uuid},
};

#[derive(Debug, Serialize, JsonSchema)]
pub struct Friend {
	player: Uuid,
	kind: RelationshipKind,
	since: DateTime<FixedOffset>,
	online: bool,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listFriends")
		.summary("List friends")
		.description("Lists every player the authenticated player has a friend relationship with.")
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
			aide::axum::routing::delete_with(self::remove, self::remove_doc),
		)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<Friend>>, SocialError> {
	use entities::{prelude::*, relationships, user};

	let rows = Relationships::find()
		.filter(
			relationships::Column::UserAId
				.eq(player.id)
				.or(relationships::Column::UserBId.eq(player.id)),
		)
		.all(&state.database)
		.await?;

	let other_ids = rows
		.iter()
		.map(|row| {
			if row.user_a_id == player.id {
				row.user_b_id
			} else {
				row.user_a_id
			}
		})
		.collect::<Vec<_>>();

	let others = User::find()
		.filter(user::Column::Id.is_in(other_ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect::<std::collections::HashMap<_, _>>();

	let online = state.realtime.connections_by_owner.read().await;

	Ok(Json(
		rows.into_iter()
			.filter_map(|row| {
				let other_id = if row.user_a_id == player.id {
					row.user_b_id
				} else {
					row.user_a_id
				};
				others.get(&other_id).map(|uuid| Friend {
					player: *uuid,
					kind: row.kind,
					since: row.created_at,
					online: online.contains_key(uuid),
				})
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn remove(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	axum::extract::Path(target): axum::extract::Path<Uuid>,
) -> Result<StatusCode, SocialError> {
	use entities::{prelude::*, relationships};

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

	crate::api::websocket::send_to_owner(&state, target.minecraft_uuid, || {
		crate::api::websocket::structs::ClientBoundPacket::FriendRemoved {
			player: player.minecraft_uuid,
		}
	})
	.await;

	Ok(StatusCode::NO_CONTENT)
}
