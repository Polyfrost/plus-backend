use axum::response::IntoResponse;
use tebex::webhooks::TebexWebhookPayload;

#[axum::debug_handler]
pub(super) async fn tebex_webhook(payload: TebexWebhookPayload) -> impl IntoResponse {
	""
}
