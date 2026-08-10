use aide::{
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::Json;
use schemars::JsonSchema;
use serde::Serialize;

use crate::api::ApiState;

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/hello", get_with(self::endpoint, self::endpoint_doc))
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HelloResponse {
	message: String,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("helloV1")
		.summary("Hello world")
		.description(
			"A placeholder endpoint that confirms the v1 API is mounted and reachable.",
		)
		.tag("hello")
}

#[tracing::instrument(level = "debug")]
async fn endpoint() -> Json<HelloResponse> {
	Json(HelloResponse {
		message: "Hello, world!".to_string(),
	})
}
