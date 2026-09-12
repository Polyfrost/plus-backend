use super::{
	PayNowClient, PayNowError,
	client::Retry,
	models::{Product, Store, UpsertProduct},
};

impl PayNowClient {
	pub(crate) async fn store(&self) -> Result<Store, PayNowError> {
		self.get_unscoped(&format!("/v1/stores/{}", self.store_id()))
			.await
	}

	/// `price` is in the store currency's minor units.
	pub(crate) async fn create_product(
		&self,
		slug: &str,
		name: &str,
		description: Option<&str>,
		price: i64,
		hidden: bool,
	) -> Result<String, PayNowError> {
		let product: Product = self
			.post(
				"/products",
				&UpsertProduct {
					slug: Some(slug),
					name: Some(name),
					description: Some(&super::catalog::storefront_description(
						name,
						description,
					)),
					price: Some(price),
					allow_one_time_purchase: Some(true),
					allow_subscription: Some(false),
					is_hidden: Some(hidden),
				},
				Retry::ConnectOnly,
			)
			.await?;

		Ok(product.id)
	}

	pub(crate) async fn find_product_by_slug(
		&self,
		slug: &str,
	) -> Result<Option<Product>, PayNowError> {
		let result = self
			.get::<Vec<Product>>(&format!("/products?slug={slug}"))
			.await;

		match result {
			Ok(products) => Ok(products.into_iter().next()),
			Err(error) if error.is_not_found() => Ok(None),
			Err(error) => Err(error),
		}
	}

	/// PayNow prices are mutable, so a discount patches rather than replaces.
	pub(crate) async fn set_product_price(
		&self,
		product_id: &str,
		price: i64,
	) -> Result<(), PayNowError> {
		self.patch::<_, serde::de::IgnoredAny>(
			&format!("/products/{product_id}"),
			&UpsertProduct {
				price: Some(price),
				..Default::default()
			},
		)
		.await?;

		Ok(())
	}

	pub(crate) async fn update_product(
		&self,
		product_id: &str,
		update: &UpsertProduct<'_>,
	) -> Result<(), PayNowError> {
		self.patch::<_, serde::de::IgnoredAny>(
			&format!("/products/{product_id}"),
			update,
		)
		.await?;

		Ok(())
	}
}
