mod analytics;
mod grants;
mod hello;
mod paynow;
mod store;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest("/checkout", paynow::checkout_router().await)
		.nest("/paynow", paynow::webhook_router().await)
		.nest("/store", store::setup_router().await)
		.merge(grants::router())
		.merge(hello::router())
		.merge(analytics::setup_router().await)
}
