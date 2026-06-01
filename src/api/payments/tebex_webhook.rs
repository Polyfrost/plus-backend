use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::TransactionProvider;
use sea_orm::{ActiveValue, TransactionError, TransactionTrait};
use serde::Serialize;
use tebex::webhooks::{TebexWebhookPayload, WebhookType};
use tracing::{Instrument, Level, debug, span, trace, warn};
use uuid::Uuid;

use crate::{
	api::{ApiState, websocket::structs::ClientBoundPacket},
	database::{DatabaseTransactionExt, DatabaseUserExt},
};

#[derive(Debug, thiserror::Error)]
pub(super) enum TebexWebhokError {
	#[error("Unable to parse customer UUID from Tebex: {0}")]
	UuidParsing(#[from] sea_orm::sqlx::types::uuid::Error),
	#[error("Unable to insert data into database: {0}")]
	DatabaseInsert(#[from] sea_orm::DbErr),
}

impl IntoResponse for TebexWebhokError {
	fn into_response(self) -> axum::response::Response {
		(
			match &self {
				TebexWebhokError::UuidParsing(_) => StatusCode::BAD_REQUEST,
				TebexWebhokError::DatabaseInsert(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

/// A response to Tebex signalling webhook success
#[derive(Serialize)]
struct SuccessfulWebhookResponse {
	id: String,
}

#[derive(Debug, Default)]
struct OwnershipGrant {
	cosmetic_ids: Vec<i32>,
	emote_ids: Vec<i32>,
}

// TODO: Proper instrumentation for this with webhook type
/// The actual webhook handler
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn endpoint(
	State(state): State<ApiState>,
	payload: TebexWebhookPayload,
) -> Result<impl IntoResponse, TebexWebhokError> {
	trace!("Tebex Webhook recieved: {payload:?}");

	// Handle individual webhooks
	match payload.webhook_type {
		// Validation should be a no-op & just return success
		WebhookType::WebhookValidation {} => (),
		WebhookType::PaymentCompleted { payment } => {
			let webhook_id = payload.id.clone();
			let grants = state
				.database
				.transaction::<_, HashMap<Uuid, OwnershipGrant>, TebexWebhokError>(
					|txn| {
						use entities::{
							cosmetic_package, emote_package, player_owned_cosmetic,
							player_owned_emote, prelude::*,
						};
						use sea_orm::prelude::*;

						Box::pin(
							async move {
								let mut grants = HashMap::<Uuid, OwnershipGrant>::new();
								let products = payment.products.iter().filter_map(|p| {
									Some((Uuid::try_parse(&p.username.id).ok()?, p.id))
								});

								for (uuid, product_id) in products {
									let user = User::get_or_create(txn, uuid).await?;
									let transaction = Transaction::get_or_create_tebex(
										txn,
										user.id,
										&payment.transaction_id,
										serde_json::json!({
											"webhook_id": webhook_id.clone(),
											"package_id": product_id,
										}),
									)
									.await?;

									let cosmetic_packages = CosmeticPackage::find()
										.filter(
											cosmetic_package::Column::PackageId
												.eq(product_id),
										)
										.all(txn)
										.await?;
									if !cosmetic_packages.is_empty() {
										PlayerOwnedCosmetic::insert_many(
											cosmetic_packages.iter().map(|package| {
												player_owned_cosmetic::ActiveModel {
													player_id: ActiveValue::Set(user.id),
													cosmetic_id: ActiveValue::Set(
														package.cosmetic_id,
													),
													acquired_via: ActiveValue::Set(
														TransactionProvider::Tebex,
													),
													transaction_id: ActiveValue::Set(
														Some(transaction.id),
													),
													..Default::default()
												}
											}),
										)
										.on_conflict_do_nothing()
										.exec(txn)
										.await?;
										grants
											.entry(uuid)
											.or_default()
											.cosmetic_ids
											.extend(
												cosmetic_packages
													.into_iter()
													.map(|p| p.cosmetic_id),
											);
									}

									let emote_packages = EmotePackage::find()
										.filter(
											emote_package::Column::PackageId
												.eq(product_id),
										)
										.all(txn)
										.await?;
									if !emote_packages.is_empty() {
										PlayerOwnedEmote::insert_many(
											emote_packages.iter().map(|package| {
												player_owned_emote::ActiveModel {
													player_id: ActiveValue::Set(user.id),
													emote_id: ActiveValue::Set(
														package.emote_id,
													),
													acquired_via: ActiveValue::Set(
														TransactionProvider::Tebex,
													),
													transaction_id: ActiveValue::Set(
														Some(transaction.id),
													),
													..Default::default()
												}
											}),
										)
										.on_conflict_do_nothing()
										.exec(txn)
										.await?;
										grants.entry(uuid).or_default().emote_ids.extend(
											emote_packages
												.into_iter()
												.map(|p| p.emote_id),
										);
									}
								}

								Ok(grants)
							}
							.instrument(span!(Level::DEBUG, "database_insert")),
						)
					},
				)
				.await
				.map_err(|e| match e {
					TransactionError::Connection(e) => e.into(),
					TransactionError::Transaction(e) => e,
				})?;

			for (player, grant) in grants {
				let connection_ids = state
					.realtime
					.connections_by_owner
					.read()
					.await
					.get(&player)
					.cloned()
					.unwrap_or_default();
				if !connection_ids.is_empty() {
					let connections = state.realtime.connections.read().await;
					for connection_id in connection_ids {
						let Some(connection) = connections.get(&connection_id) else {
							continue;
						};
						let _ = connection.tx.send(ClientBoundPacket::OwnershipUpdated {
							player,
							cosmetic_ids: grant.cosmetic_ids.clone(),
							emote_ids: grant.emote_ids.clone(),
						});
					}
				}
			}
		}
		// On unknown webhook types, log it and process as a no-op so Tebex doesn't mark
		// this webhook as failed
		WebhookType::Unknown {
			unknown_type,
			content,
		} => {
			let _span = span!(Level::WARN, "unknown_tebex_webhook_type", id = payload.id)
				.entered();

			// Ensure the webhook type is logged for debugging
			warn!("Unknown Tebex webhook type: {unknown_type}");
			debug!(
				"Webhook content: {}",
				serde_json::to_string_pretty(&content)
					.expect("infailible: was decoded from JSON")
			);
		}
	}

	// Return success response, ensuring Tebex marks the webhook as recieved
	trace!("Tebex Webhook handled successfully");
	Ok((
		StatusCode::OK,
		Json(SuccessfulWebhookResponse { id: payload.id }),
	))
}
