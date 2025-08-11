mod webhook;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().route(
		"/tebex-webhook",
		axum::routing::post(webhook::tebex_webhook_endpoint)
	)
}
