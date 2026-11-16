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
use std::collections::HashMap;

use entities::{prelude::*, user};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	permissions::{Require, perm},
	v0::{
		cosmetics::grant::{GrantError, grant_cosmetics, revoke_cosmetics},
		players::{PlayerError, lookup::resolve_username},
	},
};

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/admin/grants", post_with(self::create, self::create_doc))
		.api_route("/admin/grants/bulk", post_with(self::create_bulk, self::create_bulk_doc))
		.api_route("/admin/grants/revoke", post_with(self::revoke, self::revoke_doc))
		.api_route(
			"/admin/grants/revoke/bulk",
			post_with(self::revoke_bulk, self::revoke_bulk_doc),
		)
		.api_route(
			"/admin/grants/player/{query}",
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
			 rather than an error. Requires `GRANT_COSMETICS`.",
		)
		.tag("grants")
}

fn create_bulk_doc(op: TransformOperation) -> TransformOperation {
	op.id("createAdminGrantBulk")
		.summary("Grant cosmetics to many players")
		.description(
			"Grants every listed cosmetic to every listed player in one \
			 transaction, with the same group expansion as a single grant. An \
			 unknown cosmetic fails the whole request. At most 500 players and \
			 100 cosmetics. Requires `GRANT_COSMETICS`.",
		)
		.tag("grants")
}

fn revoke_doc(op: TransformOperation) -> TransformOperation {
	op.id("revokeAdminGrant")
		.summary("Remove a cosmetic from a player")
		.description(
			"Takes a cosmetic away from a player whatever granted it, paid \
			 purchases included, and unequips it. A grouped cosmetic removes \
			 the whole group. Removing something the player does not own is a \
			 no-op. Requires `GRANT_COSMETICS`.",
		)
		.tag("grants")
}

fn revoke_bulk_doc(op: TransformOperation) -> TransformOperation {
	op.id("revokeAdminGrantBulk")
		.summary("Remove cosmetics from many players")
		.description(
			"Removes every listed cosmetic from every listed player in one \
			 transaction, as a single removal does. At most 500 players and \
			 100 cosmetics. Requires `GRANT_COSMETICS`.",
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
			 anyone by that id has. Requires `GRANT_COSMETICS`.",
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

#[derive(Debug, Deserialize, JsonSchema)]
struct BulkRequest {
	/// The Minecraft UUIDs of the players.
	players: Vec<Uuid>,
	/// The cosmetics. Any variant id stands for its whole group.
	cosmetic_ids: Vec<i32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RevokeResponse {
	/// Every cosmetic id the player no longer owns.
	removed: Vec<i32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct BulkRevokeResponse {
	/// What each player lost, keyed by UUID. Players who owned none of it
	/// are left out.
	removed: HashMap<Uuid, Vec<i32>>,
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
	_auth: Require<{ perm::GRANT_COSMETICS }>,
	Json(body): Json<GrantRequest>,
) -> Result<Json<GrantResponse>, GrantError> {
	let granted = grant_cosmetics(&state, &[body.player], &[body.cosmetic_id]).await?;

	Ok(Json(GrantResponse { granted }))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create_bulk(
	State(state): State<ApiState>,
	_auth: Require<{ perm::GRANT_COSMETICS }>,
	Json(body): Json<BulkRequest>,
) -> Result<Json<GrantResponse>, GrantError> {
	let granted = grant_cosmetics(&state, &body.players, &body.cosmetic_ids).await?;

	Ok(Json(GrantResponse { granted }))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn revoke(
	State(state): State<ApiState>,
	_auth: Require<{ perm::GRANT_COSMETICS }>,
	Json(body): Json<GrantRequest>,
) -> Result<Json<RevokeResponse>, GrantError> {
	let removed = revoke_cosmetics(&state, &[body.player], &[body.cosmetic_id])
		.await?
		.remove(&body.player)
		.unwrap_or_default();

	Ok(Json(RevokeResponse { removed }))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn revoke_bulk(
	State(state): State<ApiState>,
	_auth: Require<{ perm::GRANT_COSMETICS }>,
	Json(body): Json<BulkRequest>,
) -> Result<Json<BulkRevokeResponse>, GrantError> {
	let removed = revoke_cosmetics(&state, &body.players, &body.cosmetic_ids).await?;

	Ok(Json(BulkRevokeResponse { removed }))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn player(
	State(state): State<ApiState>,
	_auth: Require<{ perm::GRANT_COSMETICS }>,
	Path(query): Path<String>,
) -> Result<Json<PlayerResponse>, PlayerError> {
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
