mod analytics;
mod discounts;
mod grants;
mod hello;
mod listings;
mod oneclient;
mod paynow;
mod referrals;
mod store;
mod tokens;

pub(super) use oneclient::{SyncStorage, spawn_sync_sweeper};
pub(super) use paynow::spawn_expiry_sweeper;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest("/checkout", paynow::checkout_router().await)
		.nest("/paynow", paynow::webhook_router().await)
		.nest("/oneclient", oneclient::router())
		.merge(discounts::router())
		.merge(grants::router())
		.merge(listings::router())
		.merge(referrals::router())
		.merge(store::router())
		.merge(hello::router())
		.merge(tokens::router())
		.merge(analytics::setup_router().await)
}
