use reqwest::Method;

use super::{PayNowClient, PayNowError, client::Retry, models::Created};

/// Sales and coupons live at different paths but behave the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Promotion {
	Sale,
	Coupon,
}

impl Promotion {
	fn path(self) -> &'static str {
		match self {
			Self::Sale => "/sales",
			Self::Coupon => "/coupons",
		}
	}
}

impl PayNowClient {
	pub(crate) async fn create_promotion(
		&self,
		kind: Promotion,
		body: &impl serde::Serialize,
	) -> Result<String, PayNowError> {
		let created: Created = self.post(kind.path(), body, Retry::ConnectOnly).await?;
		Ok(created.id)
	}

	pub(crate) async fn update_promotion(
		&self,
		kind: Promotion,
		id: &str,
		body: &impl serde::Serialize,
	) -> Result<(), PayNowError> {
		self.patch::<_, serde::de::IgnoredAny>(&format!("{}/{id}", kind.path()), body)
			.await?;
		Ok(())
	}

	/// Already gone counts as deleted.
	pub(crate) async fn delete_promotion(
		&self,
		kind: Promotion,
		id: &str,
	) -> Result<(), PayNowError> {
		match self
			.forward(
				Method::DELETE,
				&format!("{}/{id}", kind.path()),
				None,
				Retry::Idempotent,
			)
			.await
		{
			Err(error) if !error.is_not_found() => Err(error),
			_ => Ok(()),
		}
	}
}
