use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use tebex::webhooks::{TebexWebhookPayload, WebhookType};
use tracing::{debug, span, trace, warn, Level};

use crate::api::ApiState;

/// A response to Tebex signalling webhook success
#[derive(Serialize)]
struct SuccessfulWebhookResponse {
	id: String
}

/// The actual webhook handler
#[axum::debug_handler]
pub(super) async fn tebex_webhook(
	_state: State<ApiState>, // Necessary for payload extractor to take the state
	payload: TebexWebhookPayload
) -> impl IntoResponse {
	trace!("Tebex Webhook recieved: {payload:?}");

	// Handle individual webhooks
	match payload.webhook_type {
		// Validation should be a no-op & just return success
		WebhookType::WebhookValidation {} => (),
		// TODO: implement
		WebhookType::PaymentCompleted { payment } => {
			dbg!(payment);
		}
		// On unknown webhook types, log it and process as a no-op so Tebex doesn't mark
		// this webhook as failed
		WebhookType::Unknown {
			unknown_type,
			content
		} => {
			let _span = span!(Level::WARN, "unknown_tebex_webhook_type", id = payload.id).entered();

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
	(
		StatusCode::OK,
		Json(SuccessfulWebhookResponse { id: payload.id })
	)
}
