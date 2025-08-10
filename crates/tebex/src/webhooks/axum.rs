use axum_core::{
	extract::{FromRef, FromRequest, Request},
	response::{IntoResponse, Response}
};
use http::StatusCode;

use crate::webhooks::{WebhookValidationError, types::TebexWebhookPayload};

impl IntoResponse for WebhookValidationError {
	fn into_response(self) -> Response {
		let status = match self {
			WebhookValidationError::InvalidSignatureFormat(_) => StatusCode::BAD_REQUEST,
			WebhookValidationError::Parsing(_) => StatusCode::BAD_REQUEST,
			WebhookValidationError::Validation(_) => StatusCode::FORBIDDEN
		};

		(status, self.to_string()).into_response()
	}
}

/// State necessary for the TebexWebhookPayload extractor to function
#[derive(Debug)]
pub struct TebexWebhookState {
	pub secret: String
}

impl<S> FromRequest<S> for TebexWebhookPayload
where
	TebexWebhookState: FromRef<S>,
	String: FromRequest<S>,
	S: Send + Sync
{
	type Rejection = Response;

	#[tracing::instrument(name = "parse_tebex_webhook_req", level = "debug", skip_all)]
	async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
		let (parts, body) = req.into_parts();

		// TODO: IP validation

		let signature = parts
			.headers
			.get("X-Signature")
			.ok_or((StatusCode::UNAUTHORIZED, "X-Signature header missing"))
			.map_err(IntoResponse::into_response)?
			.to_str()
			.map_err(|_| {
				(
					StatusCode::BAD_REQUEST,
					"X-Signature header not valid ASCII"
				)
			})
			.map_err(IntoResponse::into_response)?
			.to_owned();

		let req = Request::from_parts(parts, body);
		let body = String::from_request(req, state)
			.await
			.map_err(IntoResponse::into_response)?;

		let payload = Self::validate_str(
			&body,
			&signature,
			&TebexWebhookState::from_ref(state).secret
		)
		.map_err(IntoResponse::into_response)?;

		Ok(payload)
	}
}
