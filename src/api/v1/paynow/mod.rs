mod create;
mod grant;
mod refund;
mod resolve;
mod webhook;

use aide::axum::{ApiRouter, routing::post_with};

use crate::api::ApiState;

pub(super) async fn checkout_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/create", post_with(create::endpoint, create::endpoint_doc))
}

pub(super) async fn webhook_router() -> ApiRouter<ApiState> {
	ApiRouter::new().route("/webhook", axum::routing::post(webhook::endpoint))
}
