use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::webhooks::TebexPaymentSubject;

#[derive(Debug, Deserialize)]
pub struct TebexWebhookPayload {
	/// The unique ID of this webhook payload
	pub id: String,
	/// The timestamp this webhook payload was generated
	pub date: DateTime<Utc>,
	/// The actual data being sent in this webhook call
	#[serde(flatten)]
	pub webhook_type: WebhookType,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "subject")]
pub enum WebhookType {
	/// A webhook attempting to validate if the endpoint is a valid and
	/// functional Tebex webhook handler.
	///
	/// In order to pass this validation process, the handler must respond 200
	/// OK with a JSON object containing only the ID of this webhook call.
	#[serde(rename = "validation.webhook")]
	WebhookValidation {},

	#[serde(rename = "payment.completed")]
	PaymentCompleted {
		#[serde(flatten)]
		payment: Box<TebexPaymentSubject>,
	},

	/// A catch-all for unhandled webhook types.
	#[serde(untagged)]
	Unknown {
		#[serde(rename = "type")]
		unknown_type: String,
		#[serde(rename = "subject")]
		content: HashMap<String, Value>,
	},
}
