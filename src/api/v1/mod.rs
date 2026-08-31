mod analytics;
mod hello;
mod paynow;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest("/checkout", paynow::checkout_router().await)
		.nest("/paynow", paynow::webhook_router().await)
		.merge(hello::router())
		.merge(analytics::setup_router().await)
}
