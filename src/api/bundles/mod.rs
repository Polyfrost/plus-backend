mod manage;
mod search;
mod view;

use aide::axum::ApiRouter;
use entities::bundles;
use schemars::JsonSchema;
use serde::Serialize;

use crate::api::ApiState;

/// A single enabled bundle's public information.
#[derive(Debug, Serialize, JsonSchema)]
struct BundleInfo {
	id: i32,
	name: String,
	description: Option<String>,
	asset_id: Option<i32>,
	stripe_price_id: Option<String>,
	base_price: Option<f32>,
	discount_rate: Option<i32>,
	/// The bundle's creation time, formatted as an RFC 3339 timestamp.
	created_at: String,
}

impl From<bundles::Model> for BundleInfo {
	fn from(bundle: bundles::Model) -> Self {
		BundleInfo {
			id: bundle.id,
			name: bundle.name,
			description: bundle.description,
			asset_id: bundle.asset_id,
			stripe_price_id: bundle.stripe_price_id,
			base_price: bundle.base_price,
			discount_rate: bundle.discount_rate,
			created_at: bundle.created_at.to_rfc3339(),
		}
	}
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/bundles",
		search::router()
			.merge(view::router())
			.merge(manage::router()),
	)
}
