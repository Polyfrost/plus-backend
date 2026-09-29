use reqwest::Method;

use super::{
	PayNowClient, PayNowError,
	client::Retry,
	models::{Tag, UpsertTag},
};

impl PayNowClient {
	/// Every tag on the store. Tag counts are small enough that this is one
	/// request rather than a paged walk.
	pub(crate) async fn tags(&self) -> Result<Vec<Tag>, PayNowError> {
		self.get("/tags").await
	}

	pub(crate) async fn create_tag(
		&self,
		slug: &str,
		name: &str,
		description: Option<&str>,
	) -> Result<Tag, PayNowError> {
		self.post(
			"/tags",
			&UpsertTag {
				slug: Some(slug),
				name: Some(name),
				description,
				enabled: Some(true),
			},
			Retry::ConnectOnly,
		)
		.await
	}

	pub(crate) async fn delete_tag(&self, tag_id: &str) -> Result<(), PayNowError> {
		self.forward(
			Method::DELETE,
			&format!("/tags/{tag_id}"),
			None,
			Retry::Idempotent,
		)
		.await
		.map(|_| ())
	}
}
