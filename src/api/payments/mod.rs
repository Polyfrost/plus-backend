mod webhook;

use aide::axum::ApiRouter;

pub(super) async fn setup_router<S>(state: S) -> ApiRouter<S> {
	ApiRouter::new().route(
		"/tebex-webhook",
		axum::routing::post(webhook::tebex_webhook)
	).with_state(state)
}
