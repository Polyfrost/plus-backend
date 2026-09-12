use aide::{axum::routing::get_with, transform::TransformOperation};
use axum::{Json, extract::{Path, State}};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::PlayerError;
use crate::{
	api::{ApiState, v0::account::AuthenticatedPlayer},
	database::DatabaseUserExt,
};

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("lookupPlayerByUsername")
		.summary("Look up a player by username")
		.description(
			"Resolves a case-insensitive exact username match to a player \
			 UUID. Falls back to Mojang if the player hasn't logged into \
			 PolyPlus yet, so they can still be looked up (e.g. to friend \
			 them). 404 if the username doesn't correspond to any Minecraft \
			 account.",
		)
		.tag("players")
}

#[derive(Debug, Serialize, JsonSchema)]
pub(in crate::api) struct LookupResponse {
	pub id: Uuid,
	pub username: String,
}

pub(super) fn router() -> aide::axum::ApiRouter<ApiState> {
	aide::axum::ApiRouter::new().api_route(
		"/by-username/{username}",
		get_with(self::endpoint, self::endpoint_doc),
	)
}

#[derive(Deserialize)]
struct MojangProfile {
	id: String,
	name: String,
}

async fn fetch_mojang_profile(state: &ApiState, username: &str) -> Option<MojangProfile> {
	let response = state
		.client
		.get(format!(
			"https://api.mojang.com/users/profiles/minecraft/{username}"
		))
		.send()
		.await
		.ok()?;

	if !response.status().is_success() {
		return None;
	}

	response.json::<MojangProfile>().await.ok()
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(_player): AuthenticatedPlayer,
	Path(username): Path<String>,
) -> Result<Json<LookupResponse>, PlayerError> {
	Ok(Json(resolve_username(&state, &username).await?))
}

/// Resolves a username to the player behind it, creating the player row from
/// the Mojang profile when they have never logged into PolyPlus.
pub(in crate::api) async fn resolve_username(
	state: &ApiState,
	username: &str,
) -> Result<LookupResponse, PlayerError> {
	use entities::{prelude::*, user};

	let existing = User::find()
		.filter(user::Column::Username.is_not_null())
		.filter(Expr::cust_with_values("username ILIKE $1", [username]))
		.one(&state.database)
		.await?;

	if let Some(player) = existing {
		return Ok(LookupResponse {
			id: player.minecraft_uuid,
			username: player.username.expect("filtered to non-null above"),
		});
	}

	let profile = fetch_mojang_profile(state, username)
		.await
		.ok_or(PlayerError::PlayerMissing)?;
	let uuid = Uuid::parse_str(&profile.id).map_err(|_| PlayerError::PlayerMissing)?;

	let player = User::get_or_create(&state.database, uuid).await?;
	if player.username.as_deref() != Some(profile.name.as_str()) {
		User::set_username(&state.database, player.id, &profile.name).await?;
	}

	Ok(LookupResponse {
		id: uuid,
		username: profile.name,
	})
}
