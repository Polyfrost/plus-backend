use std::{collections::HashMap, net::IpAddr};

use super::{
	PayNowClient, PayNowError,
	client::Retry,
	models::{CheckoutSession, CreateCheckout, CreateCheckoutLine},
};

impl PayNowClient {
	/// `customer_id` is the buyer; a line's gift target is the recipient.
	pub(crate) async fn create_checkout(
		&self,
		customer_id: &str,
		lines: Vec<CreateCheckoutLine>,
		return_url: &str,
		cancel_url: &str,
		metadata: HashMap<String, String>,
		customer_ip: Option<IpAddr>,
	) -> Result<CheckoutSession, PayNowError> {
		self.post_for(
			"/checkouts",
			&CreateCheckout {
				customer_id,
				lines,
				return_url,
				cancel_url,
				metadata,
			},
			Retry::ConnectOnly,
			customer_ip,
		)
		.await
	}
}
