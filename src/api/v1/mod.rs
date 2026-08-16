mod analytics;
mod hello;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(hello::router())
		.merge(analytics::setup_router().await)
}
