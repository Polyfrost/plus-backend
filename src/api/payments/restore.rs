use std::collections::{HashMap, HashSet};

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Query, State, rejection::QueryRejection},
	http::StatusCode,
	response::IntoResponse,
};
use entities::{
	cosmetic_package, emote_package, prelude::*,
	sea_orm_active_enums::TransactionProvider,
};
use schemars::JsonSchema;
use sea_orm::{ActiveValue, TransactionTrait, prelude::*};
use serde::{Deserialize, Serialize};
use tebex::apis::plugin::customer_purchases::ActivePackagesRequest;
use uuid::Uuid;

use crate::{
	api::{ApiState, websocket::structs::ClientBoundPacket},
	database::{DatabaseTransactionExt, DatabaseUserExt},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum RestoreError {
	#[error("The passed player UUID was not a valid UUID: {0}")]
	InvalidUuid(#[from] QueryRejection),
	#[error("Unable to fetch active packages for player: {0}")]
	ActivePackagesFetch(#[source] reqwest::Error),
	#[error("Unable to query database: {0}")]
	DatabaseError(#[from] sea_orm::DbErr),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("restorePayments")
		.summary("Restore all previous payments made by a player")
		.description(
			"Fetches all payments made by a player from Tebex and ensures they are \
			 properly stored in the database",
		)
		.tag("payments")
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to restore payments",
				)
			},
		)
}

impl IntoResponse for RestoreError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::InvalidUuid(_) => StatusCode::BAD_REQUEST,
				Self::ActivePackagesFetch(_) => StatusCode::INTERNAL_SERVER_ERROR,
				Self::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreQuery {
	/// The UUID of the player to attempt to restore the purchases of
	#[schemars(example = &"f7c77d999f154a66a87dc4a51ef30d19")]
	player: Uuid,
}

/// A response given on successful a payment restore
#[derive(Debug, Default, Serialize, JsonSchema)]
struct RestoreResponse {
	/// A list of all Tebex payment IDs restored
	#[schemars(example = ["tbx-42121625a15259-c182f4", "tbx-18222225a75296-bc0925"])]
	restored_ids: HashSet<String>,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/restore", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	query: Result<Query<RestoreQuery>, QueryRejection>,
) -> Result<Json<RestoreResponse>, RestoreError> {
	// Handle query errors
	let Query(query) = query?;

	// Fetch all active purchases for the player
	let active_packages = state
		.tebex
		.plugin_client
		.active_packages(ActivePackagesRequest {
			id: query
				.player
				.simple()
				.encode_lower(&mut Uuid::encode_buffer()),
			package: None,
		})
		.await
		.map_err(RestoreError::ActivePackagesFetch)?; // TODO handle 404 "invalid ID" from tebex

	let mut response = RestoreResponse::default();

	if active_packages.is_empty() {
		return Ok(Json(response));
	}

	let transactions = active_packages.iter().fold(HashMap::new(), |mut acc, ap| {
		acc.insert(ap.package.id, ap.txn_id.as_str());

		acc
	});

	let txn = state.database.begin().await?;

	// Get the user in the database (or make it if it does not exist)
	let user = User::get_or_create(&txn, query.player).await?;

	let mut granted_cosmetics = Vec::new();
	let mut granted_emotes = Vec::new();

	for (package_id, transaction_id) in &transactions {
		let transaction = Transaction::get_or_create_tebex(
			&txn,
			user.id,
			transaction_id,
			serde_json::json!({ "source": "restore", "package_id": package_id }),
		)
		.await?;
		response.restored_ids.insert((*transaction_id).to_string());

		let cosmetics = CosmeticPackage::find()
			.filter(cosmetic_package::Column::PackageId.eq(*package_id))
			.all(&txn)
			.await?;
		if !cosmetics.is_empty() {
			use entities::player_owned_cosmetic;
			PlayerOwnedCosmetic::insert_many(cosmetics.iter().map(|package| {
				player_owned_cosmetic::ActiveModel {
					player_id: ActiveValue::Set(user.id),
					cosmetic_id: ActiveValue::Set(package.cosmetic_id),
					acquired_via: ActiveValue::Set(TransactionProvider::Tebex),
					transaction_id: ActiveValue::Set(Some(transaction.id)),
					..Default::default()
				}
			}))
			.on_conflict_do_nothing()
			.exec(&txn)
			.await?;
			granted_cosmetics
				.extend(cosmetics.into_iter().map(|package| package.cosmetic_id));
		}

		let emotes = EmotePackage::find()
			.filter(emote_package::Column::PackageId.eq(*package_id))
			.all(&txn)
			.await?;
		if !emotes.is_empty() {
			use entities::player_owned_emote;
			PlayerOwnedEmote::insert_many(emotes.iter().map(|package| {
				player_owned_emote::ActiveModel {
					player_id: ActiveValue::Set(user.id),
					emote_id: ActiveValue::Set(package.emote_id),
					acquired_via: ActiveValue::Set(TransactionProvider::Tebex),
					transaction_id: ActiveValue::Set(Some(transaction.id)),
					..Default::default()
				}
			}))
			.on_conflict_do_nothing()
			.exec(&txn)
			.await?;
			granted_emotes.extend(emotes.into_iter().map(|package| package.emote_id));
		}
	}

	txn.commit().await?;

	let connection_ids = state
		.realtime
		.connections_by_owner
		.read()
		.await
		.get(&query.player)
		.cloned()
		.unwrap_or_default();
	if !connection_ids.is_empty() {
		let connections = state.realtime.connections.read().await;
		for connection_id in connection_ids {
			let Some(connection) = connections.get(&connection_id) else {
				continue;
			};
			let _ = connection.tx.send(ClientBoundPacket::OwnershipUpdated {
				player: query.player,
				cosmetic_ids: granted_cosmetics.clone(),
				emote_ids: granted_emotes.clone(),
			});
		}
	}

	Ok(Json(response))
}
