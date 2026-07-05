mod create;
mod delete;
mod update;

use aide::axum::ApiRouter;
use stripe_client::{Client as StripeClient, StripeError};
use stripe_product::price::CreatePrice;
use stripe_product::product::{CreateProduct, UpdateProduct};
use stripe_types::Currency;

use crate::api::ApiState;

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/manage",
		ApiRouter::new()
			.merge(create::router())
			.merge(update::router())
			.merge(delete::router()),
	)
}

/// Creates a Stripe product and returns its id.
async fn create_product(
	client: &StripeClient,
	name: &str,
	description: Option<&str>,
) -> Result<String, StripeError> {
	let mut request = CreateProduct::new(name);
	if let Some(description) = description {
		request = request.description(description);
	}

	Ok(request.send(client).await?.id.to_string())
}

/// Creates a USD price for a product (amount in integer cents) and returns its id.
async fn create_price(
	client: &StripeClient,
	product_id: &str,
	cents: i64,
) -> Result<String, StripeError> {
	Ok(CreatePrice::new(Currency::USD)
		.product(product_id)
		.unit_amount(cents)
		.send(client)
		.await?
		.id
		.to_string())
}

/// Sets a product's default price on Stripe.
async fn set_default_price(
	client: &StripeClient,
	product_id: &str,
	price_id: &str,
) -> Result<(), StripeError> {
	UpdateProduct::new(product_id)
		.default_price(price_id)
		.send(client)
		.await?;

	Ok(())
}

/// Converts a USD major-unit price (e.g. 4.99) to integer cents.
fn to_cents(base_price: f32) -> i64 {
	(base_price * 100.0).round() as i64
}
