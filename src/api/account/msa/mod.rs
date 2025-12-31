use aide::{OperationIo, axum::ApiRouter};
use axum::response::IntoResponse;
use http::StatusCode;

use crate::api::ApiState;

mod callback;
mod start;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum MsaError {
	#[error("Missing required query parameter")]
	MissingQuery,
	#[error("Invalid OAuth state")]
	InvalidState,
	#[error("OAuth token exchange failed")]
	TokenExchange,
	#[error("Unable to query Microsoft Graph")]
	GraphFailed,
	#[error("Unable to mint token")]
	TokenMint
}

impl IntoResponse for MsaError {
	fn into_response(self) -> axum::response::Response {
		(StatusCode::UNAUTHORIZED, self.to_string()).into_response()
	}
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(callback::router())
		.merge(start::router())
}
