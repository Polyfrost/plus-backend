use uuid::Uuid;

use super::{
	PayNowClient, PayNowError,
	client::Retry,
	models::{CreateCustomer, Customer},
};

impl PayNowClient {
	pub(crate) async fn lookup_customer(
		&self,
		minecraft_uuid: Uuid,
	) -> Result<Option<Customer>, PayNowError> {
		let result = self
			.get::<Customer>(&format!(
				"/customers/lookup?minecraft_uuid={minecraft_uuid}"
			))
			.await;

		match result {
			Ok(customer) => Ok(Some(customer)),
			Err(error) if error.is_not_found() => Ok(None),
			Err(error) => Err(error),
		}
	}

	pub(crate) async fn create_customer(
		&self,
		minecraft_uuid: Uuid,
		name: Option<&str>,
	) -> Result<Customer, PayNowError> {
		self.post(
			"/customers",
			&CreateCustomer {
				minecraft_uuid,
				minecraft_platform: "java",
				name,
			},
			Retry::ConnectOnly,
		)
		.await
	}

	pub(crate) async fn get_or_create_customer(
		&self,
		minecraft_uuid: Uuid,
		name: Option<&str>,
	) -> Result<Customer, PayNowError> {
		if let Some(customer) = self.lookup_customer(minecraft_uuid).await? {
			return Ok(customer);
		}

		match self.create_customer(minecraft_uuid, name).await {
			Ok(customer) => Ok(customer),
			// Lost a race with a concurrent checkout for the same player.
			Err(error) => match self.lookup_customer(minecraft_uuid).await? {
				Some(customer) => Ok(customer),
				None => Err(error),
			},
		}
	}
}
