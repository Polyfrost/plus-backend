use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub(crate) struct Store {
	pub id: String,
	/// ISO-4217, lowercase. Every product price is in this currency.
	pub currency: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MinecraftProfile {
	#[serde(default)]
	pub uuid: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Customer {
	pub id: String,
	#[serde(default)]
	pub minecraft_uuid: Option<Uuid>,
	#[serde(default)]
	pub minecraft: Option<MinecraftProfile>,
}

impl Customer {
	/// Reported at the top level on some shapes, nested on others.
	pub fn uuid(&self) -> Option<Uuid> {
		self.minecraft_uuid
			.or_else(|| self.minecraft.as_ref().and_then(|profile| profile.uuid))
	}
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateCustomer<'a> {
	pub minecraft_uuid: Uuid,
	pub minecraft_platform: &'a str,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub name: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Product {
	pub id: String,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct UpsertProduct<'a> {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub slug: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub name: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub description: Option<&'a str>,
	/// In the store currency's minor units.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub price: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub allow_one_time_purchase: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub allow_subscription: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub is_hidden: Option<bool>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateCheckoutLine {
	pub product_id: String,
	pub quantity: i32,
	/// Set only when the buyer is not the receiving player.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub gift_to_customer_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateCheckout<'a> {
	pub customer_id: &'a str,
	pub lines: Vec<CreateCheckoutLine>,
	pub return_url: &'a str,
	pub cancel_url: &'a str,
	pub metadata: HashMap<String, String>,
	/// PayNow rejects the whole checkout if any code is invalid.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub promo_codes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CheckoutSession {
	pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CheckoutSummary {
	#[serde(default)]
	pub metadata: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrderLine {
	pub id: String,
	pub product_id: String,
	#[serde(default)]
	pub quantity: i32,
	#[serde(default)]
	pub price: i64,
	#[serde(default)]
	pub discount_amount: i64,
	#[serde(default)]
	pub subtotal_amount: i64,
	#[serde(default)]
	pub tax_amount: i64,
	#[serde(default)]
	pub total_amount: i64,
	#[serde(default)]
	pub gift_to_customer: Option<Customer>,
	/// Present on the order read back from the API, absent on the webhook.
	#[serde(default)]
	pub refunded_amount: Option<i64>,
	#[serde(default)]
	pub refund_status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Order {
	pub id: String,
	pub status: String,
	#[serde(default)]
	pub currency: Option<String>,
	#[serde(default)]
	pub discount_amount: i64,
	#[serde(default)]
	pub total_amount: i64,
	#[serde(default)]
	pub customer: Option<Customer>,
	#[serde(default)]
	pub checkout: Option<CheckoutSummary>,
	#[serde(default)]
	pub lines: Vec<OrderLine>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Refund {
	#[serde(default)]
	pub refund_amount: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Payment {
	pub order_id: Option<String>,
	#[serde(default)]
	pub refund_status: Option<String>,
	#[serde(default)]
	pub refunded_at: Option<DateTime<Utc>>,
	#[serde(default)]
	pub refunds: Vec<Refund>,
	#[serde(default)]
	pub chargeback_status: Option<String>,
	#[serde(default)]
	pub chargeback_at: Option<DateTime<Utc>>,
}

impl Payment {
	pub fn refunded_total(&self) -> i64 {
		self.refunds.iter().map(|refund| refund.refund_amount).sum()
	}
}

#[derive(Debug, Deserialize)]
pub(crate) struct WebhookEnvelope {
	pub event_type: String,
	pub event_id: String,
	pub body: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ApiErrorBody {
	#[serde(default)]
	pub code: Option<String>,
	#[serde(default)]
	pub message: Option<String>,
}
