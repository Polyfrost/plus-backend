mod create;
mod webhook;

use aide::axum::{ApiRouter, routing::post_with};

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/create", post_with(create::endpoint, create::endpoint_doc))
		.route("/webhook", axum::routing::post(webhook::endpoint))
}
