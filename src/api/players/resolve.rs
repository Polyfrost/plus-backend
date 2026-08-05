use aide::{axum::routing::post_with, transform::TransformOperation};
use axum::{Json, extract::State};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::PlayerError;
use crate::api::{ApiState, account::AuthenticatedPlayer};

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

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(_player): AuthenticatedPlayer,
	Json(mut body): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, PlayerError> {
	use entities::{prelude::*, user};

	body.ids.truncate(MAX_IDS);

	let players = User::find()
		.filter(user::Column::MinecraftUuid.is_in(body.ids))
		.filter(user::Column::Username.is_not_null())
		.all(&state.database)
		.await?;

	Ok(Json(ResolveResponse {
		players: players
			.into_iter()
			.filter_map(|player| {
				Some(ResolvedPlayer {
					id: player.minecraft_uuid,
					username: player.username?,
				})
			})
			.collect(),
	}))
}
