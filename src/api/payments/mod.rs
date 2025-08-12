mod restore;
mod tebex_webhook;

use aide::axum::{ApiRouter, routing::post};

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.route(
			"/tebex-webhook",
			axum::routing::post(tebex_webhook::endpoint)
		)
		.merge(restore::router())
}
