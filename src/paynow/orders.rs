use super::{PayNowClient, PayNowError, models::Order};

impl PayNowClient {
	pub(crate) async fn order(&self, order_id: &str) -> Result<Order, PayNowError> {
		self.get(&format!("/orders/{order_id}")).await
	}
}
