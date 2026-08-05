use aide::{axum::routing::put_with, transform::TransformOperation};
use axum::{Json, extract::State, http::StatusCode};
use entities::sea_orm_active_enums::PlayerRole;
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use uuid::Uuid;

use super::PlayerError;
use crate::api::{ApiState, account::AdminPlayer};

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("setPlayerRole")
		.summary("Set a player role")
		.description("Sets a player's role. Admin role required.")
		.tag("players")
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RoleRequest {
	player: Uuid,
	role: PlayerRole,
}

pub(super) fn router() -> aide::axum::ApiRouter<ApiState> {
	aide::axum::ApiRouter::new().api_route("/role", put_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AdminPlayer(_admin): AdminPlayer,
	Json(body): Json<RoleRequest>,
) -> Result<StatusCode, PlayerError> {
	use entities::{prelude::*, user};

	let Some(player) = User::find()
		.filter(user::Column::MinecraftUuid.eq(body.player))
		.one(&state.database)
		.await?
	else {
		return Err(PlayerError::PlayerMissing);
	};

	let mut player: user::ActiveModel = player.into();
	player.role = Set(body.role);
	player.update(&state.database).await?;

	Ok(StatusCode::NO_CONTENT)
}
