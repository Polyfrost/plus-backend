use std::{collections::HashMap, net::IpAddr};

use super::{
	PayNowClient, PayNowError,
	client::Retry,
	models::{CheckoutSession, CreateCheckout, CreateCheckoutLine},
};

/// `customer_id` is the buyer; a line's gift target is the recipient.
pub(crate) struct NewCheckout<'a> {
	pub customer_id: &'a str,
	pub lines: Vec<CreateCheckoutLine>,
	pub promo_codes: Vec<String>,
	pub return_url: &'a str,
	pub cancel_url: &'a str,
	pub metadata: HashMap<String, String>,
	pub customer_ip: Option<IpAddr>,
}

impl PayNowClient {
	pub(crate) async fn create_checkout(
		&self,
		checkout: NewCheckout<'_>,
	) -> Result<CheckoutSession, PayNowError> {
		self.post_for(
			"/checkouts",
			&CreateCheckout {
				customer_id: checkout.customer_id,
				lines: checkout.lines,
				return_url: checkout.return_url,
				cancel_url: checkout.cancel_url,
				metadata: checkout.metadata,
				promo_codes: checkout.promo_codes,
			},
			Retry::ConnectOnly,
			checkout.customer_ip,
		)
		.await
	}
}
