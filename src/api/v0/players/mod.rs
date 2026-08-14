mod lookup;
mod resolve;
mod role;

use aide::{OperationIo, axum::ApiRouter};
use axum::{http::StatusCode, response::IntoResponse};

use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum PlayerError {
	#[error("The requested player does not exist")]
	PlayerMissing,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for PlayerError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::PlayerMissing => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/players",
		ApiRouter::new()
			.merge(role::router())
			.merge(resolve::router())
			.merge(lookup::router()),
	)
}
