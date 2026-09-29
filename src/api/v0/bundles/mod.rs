mod manage;
mod search;
mod view;

use aide::axum::ApiRouter;
use entities::bundles;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{api::ApiState, pricing::display::SaleInfo, product_settings::SettingsInfo};

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/bundles",
		search::router()
			.merge(view::router())
			.merge(manage::router()),
	)
}

/// A single enabled bundle's public information.
#[derive(Debug, Serialize, JsonSchema)]
struct BundleInfo {
	id: i32,
	name: String,
	description: Option<String>,
	asset_id: Option<i32>,
	store_product_id: Option<String>,
	base_price: Option<f32>,
	/// The sale this bundle is currently in, if any. `base_price` stays the
	/// list price so the client can strike it through.
	#[serde(skip_serializing_if = "Option::is_none")]
	sale: Option<SaleInfo>,
	/// When and to whom this can be sold. Absent when nothing restricts it.
	#[serde(skip_serializing_if = "Option::is_none")]
	settings: Option<SettingsInfo>,
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
			store_product_id: bundle.store_product_id,
			base_price: bundle.base_price,
			sale: None,
			settings: None,
			created_at: bundle.created_at.to_rfc3339(),
		}
	}
}
