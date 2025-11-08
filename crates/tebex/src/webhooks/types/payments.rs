use std::collections::HashMap;

use chrono::{DateTime, Utc};
use iso_country::Country;
use iso_currency::Currency;
use serde::Deserialize;
use serde_json::Value;
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
	pub coupons: Vec<TebexCouponUse>,
	pub gift_cards: Vec<TebexGiftCardUse>,
	pub recurring_payment_reference: Option<String>,
	pub custom: Option<HashMap<String, Value>>, // TODO: find type?
	// pub revenue_share: [], // TODO: find type
	pub decline_reason: Option<TebexDeclineReason>,
	pub creator_code: Option<String>
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexCouponUse {
	pub id: u32,
	pub code: String,
	#[serde(rename = "type")]
	pub coupon_type: String, // TODO: enum ("cart", ...)
	#[serde(flatten)]
	pub discount_type: TebexDiscountType
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "discount_type")]
pub enum TebexDiscountType {
	#[serde(rename = "percentage")]
	Percentage { discount_percentage: u8 },
	#[serde(rename = "value")]
	Value { discount_amount: f32 }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexGiftCardUse {
	pub card_number: String,
	pub amount: TebexCost
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
	/// The cost of the product itself
	pub base_price: TebexCost,
	/// The cost the customer actually paid for this product, such as after
	/// coupons or discounts
	pub paid_price: TebexCost,
	// pub variables: [], // TODO: find type
	pub expires_at: Option<DateTime<Utc>>,
	pub custom: Option<String>,          // TODO: verify if nullable
	pub username: TebexUsername  // pub servers: [] // TODO: find type
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
	pub postal_code: String
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexUsername {
	pub id: String,
	pub username: String
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
	pub base_currency: Option<Currency>,
	pub base_currency_price: Option<f32>
}

#[derive(Debug, Clone, Deserialize)]
pub struct TebexFees {
	pub tax: TebexCost,
	pub gateway: TebexCost
}
