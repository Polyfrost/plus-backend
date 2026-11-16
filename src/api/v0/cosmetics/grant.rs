use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::{
	cosmetic,
	prelude::*,
	sea_orm_active_enums::{OwnershipEventKind, TransactionProvider, TransactionStatus},
	transaction,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set,
	TransactionTrait,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
	api::{
		ApiState, error_response,
		permissions::{Require, perm},
		v0::websocket::structs::ClientBoundPacket,
	},
	database::{DatabaseUserExt, record_ownership_events},
	ownership,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum GrantError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for GrantError {
	fn into_response(self) -> axum::response::Response {
		error_response(
			match self {
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("grantCosmetic")
		.summary("Grant a cosmetic to a player")
		.description("Grants cosmetic ownership to a player. Admin role required.")
		.tag("cosmetics")
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrantRequest {
	player: Uuid,
	cosmetic_id: i32,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/grant", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: Require<{ perm::GRANT_COSMETICS }>,
	Json(body): Json<GrantRequest>,
) -> Result<StatusCode, GrantError> {
	grant_cosmetic(&state, body.player, body.cosmetic_id).await?;

	Ok(StatusCode::NO_CONTENT)
}

/// Grants a cosmetic to a player and tells any connected client about it.
pub(in crate::api) async fn grant_cosmetic(
	state: &ApiState,
	player_uuid: Uuid,
	cosmetic_id: i32,
) -> Result<Vec<i32>, GrantError> {
	let txn = state.database.begin().await?;
	let player = User::get_or_create(&txn, player_uuid).await?;
	let cosmetic_ids =
		grant_in(&txn, player.id, cosmetic_id, json!({ "reason": "admin_grant" })).await?;
	txn.commit().await?;

	notify_granted(state, player_uuid, &cosmetic_ids).await;

	Ok(cosmetic_ids)
}

/// Grants a cosmetic, and the rest of its group, inside `txn`. Call
/// [`notify_granted`] once it commits.
pub(in crate::api) async fn grant_in(
	txn: &DatabaseTransaction,
	player_id: i32,
	cosmetic_id: i32,
	metadata: Value,
) -> Result<Vec<i32>, GrantError> {
	let Some(cosmetic) = Cosmetic::find_by_id(cosmetic_id).one(txn).await? else {
		return Err(GrantError::MissingCosmetic);
	};

	// Buying a cosmetic grants the whole group: if this cosmetic belongs to a
	// group, every variant under that group is granted at once. Ungrouped
	// cosmetics grant just themselves.
	let cosmetic_ids: Vec<i32> = match cosmetic.group_id {
		Some(group_id) => Cosmetic::find()
			.filter(cosmetic::Column::GroupId.eq(group_id))
			.all(txn)
			.await?
			.into_iter()
			.map(|c| c.id)
			.collect(),
		None => vec![cosmetic.id],
	};

	let transaction = transaction::ActiveModel {
		player_id: Set(player_id),
		provider: Set(TransactionProvider::AdminGrant),
		provider_transaction_id: Set(None),
		status: Set(TransactionStatus::Completed),
		raw_metadata: Set(metadata),
		..Default::default()
	}
	.insert(txn)
	.await?;

	// Owned for good, including over a rental the player already holds.
	ownership::record(
		txn,
		player_id,
		&cosmetic_ids,
		TransactionProvider::AdminGrant,
		Some(transaction.id),
		None,
		None,
	)
	.await?;
	ownership::settle(txn, player_id, &cosmetic_ids).await?;

	record_ownership_events(
		txn,
		player_id,
		&cosmetic_ids,
		OwnershipEventKind::Granted,
		TransactionProvider::AdminGrant,
		Some(transaction.id),
		None,
		None,
	)
	.await?;

	Ok(cosmetic_ids)
}

pub(in crate::api) async fn notify_granted(
	state: &ApiState,
	player_uuid: Uuid,
	cosmetic_ids: &[i32],
) {
	let connection_ids = state
		.realtime
		.connections_by_owner
		.read()
		.await
		.get(&player_uuid)
		.cloned()
		.unwrap_or_default();
	if connection_ids.is_empty() {
		return;
	}

	let connections = state.realtime.connections.read().await;
	for connection_id in connection_ids {
		let Some(connection) = connections.get(&connection_id) else {
			continue;
		};
		let _ = connection.tx.send(ClientBoundPacket::OwnershipUpdated {
			player: player_uuid,
			cosmetic_ids: cosmetic_ids.to_vec(),
			emote_ids: Vec::new(),
			revoked: false,
		});
	}
}
