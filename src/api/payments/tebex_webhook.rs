use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sea_orm::{ActiveValue, TransactionError, TransactionTrait};
use serde::Serialize;
use tebex::webhooks::{TebexWebhookPayload, WebhookType};
use tracing::{Instrument, Level, debug, span, trace, warn};
use uuid::Uuid;

use crate::{api::ApiState, database::DatabaseUserExt};

#[derive(Debug, thiserror::Error)]
pub(super) enum TebexWebhokError {
	#[error("Unable to parse customer UUID from Tebex: {0}")]
	UuidParsing(#[from] sea_orm::sqlx::types::uuid::Error),
	#[error("Unable to insert data into database: {0}")]
	DatabaseInsert(#[from] sea_orm::DbErr)
}

impl IntoResponse for TebexWebhokError {
	fn into_response(self) -> axum::response::Response {
		(
			match &self {
				TebexWebhokError::UuidParsing(_) => StatusCode::BAD_REQUEST,
				TebexWebhokError::DatabaseInsert(_) => StatusCode::INTERNAL_SERVER_ERROR
			},
			self.to_string()
		)
			.into_response()
	}
}

/// A response to Tebex signalling webhook success
#[derive(Serialize)]
struct SuccessfulWebhookResponse {
	id: String
}

// TODO: Proper instrumentation for this with webhook type
/// The actual webhook handler
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn endpoint(
	State(state): State<ApiState>,
	payload: TebexWebhookPayload
) -> Result<impl IntoResponse, TebexWebhokError> {
	trace!("Tebex Webhook recieved: {payload:?}");

	// Handle individual webhooks
	match payload.webhook_type {
		// Validation should be a no-op & just return success
		WebhookType::WebhookValidation {} => (),
		WebhookType::PaymentCompleted { payment } => {
			let cosmetics = payment.products.into_iter().fold(
				HashMap::<Uuid, Vec<_>>::new(),
				|mut acc, product| {
					if let Some(id) = product
						.custom
						.strip_prefix("plus:cosmetic:")
						.and_then(|id| id.parse().ok())
						&& let Ok(uuid) = Uuid::try_parse(&product.username.id)
					{
						acc.entry(uuid)
							.or_default()
							.push((id, payment.transaction_id.clone()));
					};

					acc
				}
			);

			state
				.database
				.transaction::<_, (), TebexWebhokError>(|txn| {
					use entities::{prelude::*, user_cosmetic};
					use sea_orm::prelude::*;

					Box::pin(
						async move {
							for (uuid, cosmetics) in cosmetics.into_iter() {
								// Ensure user exists
								let user = User::get_or_create(txn, uuid).await?;

								// Create UserCosmetic(s)
								UserCosmetic::insert_many(cosmetics.into_iter().map(
									|(id, txn_id)| user_cosmetic::ActiveModel {
										user: ActiveValue::Set(user.id),
										cosmetic: ActiveValue::Set(id),
										transaction_id: ActiveValue::Set(txn_id)
									}
								))
								.on_conflict_do_nothing()
								.exec(txn)
								.await?;
							}

							Ok(())
						}
						.instrument(span!(Level::DEBUG, "database_insert"))
					)
				})
				.await
				.map_err(|e| match e {
					TransactionError::Connection(e) => e.into(),
					TransactionError::Transaction(e) => e
				})?;
		}
		// On unknown webhook types, log it and process as a no-op so Tebex doesn't mark
		// this webhook as failed
		WebhookType::Unknown {
			unknown_type,
			content
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
		Json(SuccessfulWebhookResponse { id: payload.id })
	))
}
