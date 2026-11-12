//! Lets dashboard staff answer Special Chat as one of its target accounts.

use std::time::Duration;

use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::{prelude::*, user};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	permissions::{Require, perm},
	v0::account::{TokenAccess, mint_token},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SpecialChatError {
	#[error("That player is not a Special Chat target")]
	NotATarget,
	#[error("Unable to construct token: {0}")]
	Token(#[from] pasetors::errors::Error),
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for SpecialChatError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::NotATarget => StatusCode::NOT_FOUND,
				Self::Token(_) | Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
struct Target {
	uuid: Uuid,
	username: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TokenRequest {
	uuid: Uuid,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TokenResponse {
	/// A 2h bearer token for the target, limited to reading and sending group
	/// messages, resolving names and a receive-only websocket.
	token: String,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/admin/special-chat/targets",
			get_with(self::targets, |op| {
				op.id("listSpecialChatTargets")
					.summary("List the accounts Special Chat delivers to")
					.tag("special-chat")
			}),
		)
		.api_route(
			"/admin/special-chat/token",
			post_with(self::token, |op| {
				op.id("mintSpecialChatToken")
					.summary("Mint a token to answer Special Chat as a target")
					.tag("special-chat")
			}),
		)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn targets(
	State(state): State<ApiState>,
	_auth: Require<{ perm::USE_SPECIAL_CHAT }>,
) -> Result<Json<Vec<Target>>, SpecialChatError> {
	let known = User::find()
		.filter(user::Column::MinecraftUuid.is_in(state.special_chat_targets.clone()))
		.all(&state.database)
		.await?;

	Ok(Json(
		state
			.special_chat_targets
			.iter()
			.map(|&uuid| Target {
				uuid,
				username: known
					.iter()
					.find(|player| player.minecraft_uuid == uuid)
					.and_then(|player| player.username.clone()),
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn token(
	State(state): State<ApiState>,
	Require(staff, _): Require<{ perm::USE_SPECIAL_CHAT }>,
	Json(body): Json<TokenRequest>,
) -> Result<Json<TokenResponse>, SpecialChatError> {
	if !state.special_chat_targets.contains(&body.uuid) {
		return Err(SpecialChatError::NotATarget);
	}

	tracing::info!(target = %body.uuid, by = %staff, "Minted a Special Chat token");
	// Scoped, so it cannot be used for the target's roles or anything but chat.
	let token = mint_token(
		&state,
		body.uuid,
		false,
		Duration::from_secs(60 * 60 * 2),
		TokenAccess::SpecialChat,
	)?;

	Ok(Json(TokenResponse { token }))
}
