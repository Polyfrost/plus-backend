use chrono::{DateTime, Utc};
use iso_country::Country;
use iso_currency::Currency;
use serde::Deserialize;
use serde_repr::Deserialize_repr;

#[derive(Debug, Clone, Deserialize)]
pub struct TebexPaymentSubject {
	pub transaction_id: String,
	pub status: TebexPaymentStatus,
	pub payment_sequence: String, // TODO: enum
	pub created_at: DateTime<Utc>,
	pub price: TebexCost,
	pub price_paid: TebexCost,
	pub payment_method: TebexPaymentMethod,
	pub fees: TebexFees,
	pub customer: TebexCustomer,
	pub products: Vec<TebexProduct>,
	// pub coupons: [], // TODO: find type
	// pub gift_cards: [], // TODO: find type
	pub recurring_payment_reference: Option<String>,
	// pub custom: {}, // TODO: find type
	// pub revenue_share: [], // TODO: find type
	pub decline_reason: Option<TebexDeclineReason>,
	// pub creator_code: null, // TODO: find type
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexDeclineReason {
	// TODO: enum https://docs.tebex.io/developers/webhooks/overview#decline-reasons
	pub code: String,
	pub message: String
}

#[derive(Debug, Clone, Deserialize_repr)]
#[repr(u8)]
pub enum TebexPaymentStatusCode {
	// https://docs.tebex.io/developers/webhooks/overview#useful-status-ids
	Complete = 1,
	Refund = 2,
	Chargeback = 3,
	Declined = 18,
	PendingCheckout = 19,
	RefundPending = 21
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexPaymentStatus {
	pub id: TebexPaymentStatusCode,
	pub description: String
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexProduct {
    pub id: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub product_type: String, // TODO: enum
    pub quantity: u32,
    pub base_price: TebexCost,
    pub paid_price: TebexCost,
    // pub variables: [], // TODO: find type
    pub expires_at: Option<DateTime<Utc>>,
    pub custom: String, // TODO: verify if nullable
    pub username: TebexUsername,
    // pub servers: [] // TODO: find type
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexCustomer {
	pub first_name: String,
	pub last_name: String,
	pub email: String,
	pub ip: String,
	pub username: TebexUsername,
	pub marketing_consent: bool,
	pub country: Country,
	pub postal_code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexUsername {
	pub id: String,
	pub username: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexPaymentMethod {
	pub name: String,
	pub refundable: bool
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexCost {
	pub amount: f32,
	pub currency: Currency,
	pub base_currency: Currency,
	pub base_currency_price: f32
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexFees {
	pub tax: TebexCost,
	pub gateway: TebexCost
}
