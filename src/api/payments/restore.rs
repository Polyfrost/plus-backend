use aide::{axum::{routing::post, ApiRouter, IntoApiResponse}, OperationIo};
use axum::{extract::{Query, State}, response::IntoResponse};
use schemars::JsonSchema;
use sea_orm::prelude::Uuid;
use serde::Deserialize;
use tebex::apis::plugin::customer_purchases::ActivePackagesRequest;

use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum RestoreError {
	#[error("Unable to fetch active packages for player: {0}")]
	ActivePackagesFetch(#[source] reqwest::Error)
}

// impl IntoResponse for RestoreError {
// 	fn into_response(self) -> axum::response::Response {

// 	}
// }

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreQuery {
	/// The UUID of the player to attempt to restore the purchases of
	player: Uuid
}

// #[derive(Debug, Deserialize, JsonSchema)]
// struct RestoreResponse {
// 	/// The UUID of the player to attempt to restore the purchases of
// 	player: Uuid
// }

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/restore", post(self::endpoint))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	Query(query): Query<RestoreQuery>
) -> impl IntoApiResponse {
	// Fetch all active purchases for the player
	let active_packages = state
		.tebex
		.plugin_client
		.active_packages(ActivePackagesRequest {
			id: query
				.player
				.simple()
				.encode_lower(&mut Uuid::encode_buffer()),
			package: None
		})
		.await
		.map_err(RestoreError::ActivePackagesFetch)?;

	()
}
