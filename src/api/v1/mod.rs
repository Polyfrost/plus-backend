mod analytics;
mod discounts;
mod grants;
mod hello;
mod listings;
mod paynow;

pub(super) use paynow::spawn_expiry_sweeper;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest("/checkout", paynow::checkout_router().await)
		.nest("/paynow", paynow::webhook_router().await)
		.merge(discounts::router())
		.merge(grants::router())
		.merge(listings::router())
		.merge(hello::router())
		.merge(analytics::setup_router().await)
}
