use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::{TransactionProvider, TransactionStatus};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ActiveValue, EntityTrait, Set, TransactionTrait};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
	api::{ApiState, account::AdminPlayer, websocket::structs::ClientBoundPacket},
	database::DatabaseUserExt,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum GrantError {
	#[error("The requested emote does not exist")]
	MissingEmote,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for GrantError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::MissingEmote => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("grantEmote")
		.summary("Grant an emote to a player")
		.description("Grants emote ownership to a player. Admin role required.")
		.tag("emotes")
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrantRequest {
	player: Uuid,
	emote_id: i32,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/emote/grant",
		post_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AdminPlayer(_admin): AdminPlayer,
	Json(body): Json<GrantRequest>,
) -> Result<StatusCode, GrantError> {
	use entities::{player_owned_emote, prelude::*, transaction};

	let txn = state.database.begin().await?;
	let Some(_) = Emote::find_by_id(body.emote_id).one(&txn).await? else {
		return Err(GrantError::MissingEmote);
	};
	let player = User::get_or_create(&txn, body.player).await?;
	let transaction = transaction::ActiveModel {
		player_id: Set(player.id),
		provider: Set(TransactionProvider::AdminGrant),
		provider_transaction_id: Set(None),
		status: Set(TransactionStatus::Completed),
		raw_metadata: Set(serde_json::json!({ "reason": "admin_grant" })),
		..Default::default()
	}
	.insert(&txn)
	.await?;

	PlayerOwnedEmote::insert(player_owned_emote::ActiveModel {
		player_id: Set(player.id),
		emote_id: Set(body.emote_id),
		acquired_via: Set(TransactionProvider::AdminGrant),
		transaction_id: Set(Some(transaction.id)),
		acquired_at: ActiveValue::NotSet,
	})
	.on_conflict_do_nothing()
	.exec(&txn)
	.await?;
	txn.commit().await?;

	let connection_ids = state
		.realtime
		.connections_by_owner
		.read()
		.await
		.get(&body.player)
		.cloned()
		.unwrap_or_default();
	if !connection_ids.is_empty() {
		let connections = state.realtime.connections.read().await;
		for connection_id in connection_ids {
			let Some(connection) = connections.get(&connection_id) else {
				continue;
			};
			let _ = connection.tx.send(ClientBoundPacket::OwnershipUpdated {
				player: body.player,
				cosmetic_ids: Vec::new(),
				emote_ids: vec![body.emote_id],
			});
		}
	}

	Ok(StatusCode::NO_CONTENT)
}
