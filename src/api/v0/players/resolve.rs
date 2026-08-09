use aide::{axum::routing::post_with, transform::TransformOperation};
use axum::{Json, extract::State};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::PlayerError;
use crate::{
	api::{ApiState, v0::account::AuthenticatedPlayer},
	database::DatabaseUserExt,
};

const MAX_IDS: usize = 100;

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("resolvePlayers")
		.summary("Resolve player UUIDs to usernames")
		.description(
			"Batch-resolves up to 100 Minecraft UUIDs to their last-known \
			 username. UUIDs with no known username (never logged in) or that \
			 don't exist are simply omitted from the response.",
		)
		.tag("players")
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ResolveRequest {
	ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ResolvedPlayer {
	id: Uuid,
	username: String,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
struct ResolveResponse {
	players: Vec<ResolvedPlayer>,
}

pub(super) fn router() -> aide::axum::ApiRouter<ApiState> {
	aide::axum::ApiRouter::new()
		.api_route("/resolve", post_with(self::endpoint, self::endpoint_doc))
}

async fn fetch_mojang_username(state: &ApiState, uuid: Uuid) -> Option<String> {
	#[derive(Deserialize)]
	struct MojangProfile {
		name: String,
	}

	let response = state
		.client
		.get(format!(
			"https://sessionserver.mojang.com/session/minecraft/profile/{}",
			uuid.as_simple()
		))
		.send()
		.await
		.ok()?;

	if !response.status().is_success() {
		return None;
	}

	response.json::<MojangProfile>().await.ok().map(|profile| profile.name)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(_player): AuthenticatedPlayer,
	Json(mut body): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, PlayerError> {
	use entities::{prelude::*, user};

	body.ids.truncate(MAX_IDS);

	let rows = User::find()
		.filter(user::Column::MinecraftUuid.is_in(body.ids))
		.all(&state.database)
		.await?;

	let mut players = Vec::with_capacity(rows.len());
	for row in rows {
		match row.username {
			Some(username) => players.push(ResolvedPlayer {
				id: row.minecraft_uuid,
				username,
			}),
			None if state.special_chat_targets.contains(&row.minecraft_uuid) => {
				if let Some(username) = fetch_mojang_username(&state, row.minecraft_uuid).await {
					let _ = User::set_username(&state.database, row.id, &username).await;
					players.push(ResolvedPlayer {
						id: row.minecraft_uuid,
						username,
					});
				}
			}
			None => {}
		}
	}

	Ok(Json(ResolveResponse { players }))
}
