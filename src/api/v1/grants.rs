use aide::{
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	admin_auth::AdminAuthenticationExtractor,
	v0::{
		cosmetics::grant::{GrantError, grant_cosmetic},
		players::{PlayerError, lookup::resolve_username},
	},
};

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/grants", post_with(self::create, self::create_doc))
		.api_route(
			"/grants/player/{query}",
			get_with(self::player, self::player_doc),
		)
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createAdminGrant")
		.summary("Grant a cosmetic to a player")
		.description(
			"Grants cosmetic ownership to a player, recorded as an admin \
			 grant. A cosmetic that belongs to a group grants every variant \
			 in that group, so the response usually lists more ids than were \
			 asked for. Granting something the player already owns is a no-op \
			 rather than an error. Admin password required.",
		)
		.tag("grants")
}

fn player_doc(op: TransformOperation) -> TransformOperation {
	op.id("resolveGrantPlayer")
		.summary("Resolve a grant target")
		.description(
			"Resolves a UUID or a case-insensitive username to the player a \
			 grant would land on, so the dashboard can confirm the target \
			 before granting. A username falls back to Mojang when the player \
			 has never logged into PolyPlus; a UUID is accepted whether or not \
			 anyone by that id has. Admin password required.",
		)
		.tag("grants")
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrantRequest {
	/// The Minecraft UUID of the player to grant to.
	player: Uuid,
	/// The cosmetic to grant. Any variant id grants the whole group.
	cosmetic_id: i32,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GrantResponse {
	/// Every cosmetic id the player now owns as a result of this grant.
	granted: Vec<i32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PlayerResponse {
	id: Uuid,
	/// Absent for a UUID belonging to someone who has never logged in, since
	/// there is no name on record to show.
	#[serde(skip_serializing_if = "Option::is_none")]
	username: Option<String>,
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<GrantRequest>,
) -> Result<Json<GrantResponse>, GrantError> {
	let granted = grant_cosmetic(&state, body.player, body.cosmetic_id).await?;

	Ok(Json(GrantResponse { granted }))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn player(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(query): Path<String>,
) -> Result<Json<PlayerResponse>, PlayerError> {
	use entities::{prelude::*, user};

	let query = query.trim();

	if let Ok(uuid) = Uuid::parse_str(query) {
		let existing = User::find()
			.filter(user::Column::MinecraftUuid.eq(uuid))
			.one(&state.database)
			.await?;

		return Ok(Json(PlayerResponse {
			id: uuid,
			username: existing.and_then(|player| player.username),
		}));
	}

	let found = resolve_username(&state, query).await?;

	Ok(Json(PlayerResponse {
		id: found.id,
		username: Some(found.username),
	}))
}
