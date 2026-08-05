use aide::{axum::routing::get_with, transform::TransformOperation};
use axum::{Json, extract::{Path, State}};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
use serde::Serialize;
use uuid::Uuid;

use super::PlayerError;
use crate::api::{ApiState, account::AuthenticatedPlayer};

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("lookupPlayerByUsername")
		.summary("Look up a player by username")
		.description(
			"Resolves a case-insensitive exact username match to a player \
			 UUID. Only players who have logged in at least once are \
			 resolvable. 404 if there's no match.",
		)
		.tag("players")
}

#[derive(Debug, Serialize, JsonSchema)]
struct LookupResponse {
	id: Uuid,
	username: String,
}

pub(super) fn router() -> aide::axum::ApiRouter<ApiState> {
	aide::axum::ApiRouter::new().api_route(
		"/by-username/{username}",
		get_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(_player): AuthenticatedPlayer,
	Path(username): Path<String>,
) -> Result<Json<LookupResponse>, PlayerError> {
	use entities::{prelude::*, user};

	let player = User::find()
		.filter(user::Column::Username.is_not_null())
		.filter(Expr::cust_with_values("username ILIKE $1", [username]))
		.one(&state.database)
		.await?
		.ok_or(PlayerError::PlayerMissing)?;

	Ok(Json(LookupResponse {
		id: player.minecraft_uuid,
		username: player.username.expect("filtered to non-null above"),
	}))
}
