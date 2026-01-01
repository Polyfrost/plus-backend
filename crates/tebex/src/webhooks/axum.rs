use std::borrow::Cow;

use axum_core::{
	extract::{FromRef, FromRequest, FromRequestParts, Request},
	response::{IntoResponse, Response},
};
use http::StatusCode;

use crate::webhooks::{WebhookValidationError, types::TebexWebhookPayload};

mod errors {
	use http::StatusCode;

	pub(super) const MISSING_SIGNATURE_HEADER: (StatusCode, &str) =
		(StatusCode::UNAUTHORIZED, "X-Signature header missing");
	pub(super) const INVALID_SIGNATURE_HEADER: (StatusCode, &str) = (
		StatusCode::BAD_REQUEST,
		"X-Signature header not valid ASCII",
	);

	#[cfg(feature = "validate-source-ip")]
	pub(super) const INCORRECT_SOURCE_IP: (StatusCode, &str) = (
		StatusCode::UNAUTHORIZED,
		"Source IP is not allowed to access this endpoint",
	);
}

impl IntoResponse for WebhookValidationError {
	fn into_response(self) -> Response {
		let status = match self {
			WebhookValidationError::InvalidSignatureFormat(_) => StatusCode::BAD_REQUEST,
			WebhookValidationError::Parsing(_) => StatusCode::BAD_REQUEST,
			WebhookValidationError::Validation(_) => StatusCode::FORBIDDEN,
		};

		(status, self.to_string()).into_response()
	}
}

/// State necessary for the TebexWebhookPayload extractor to function
#[derive(Debug)]
pub struct TebexWebhookState {
	/// The secret Tebex will sign requests with, located in the webhooks
	/// configuration.
	///
	/// This is a [Cow<'static, str>] to allow for owned strings to be passed,
	/// while also allowing a static value to be passed (usually with
	/// [Box::leak])
	pub secret: Cow<'static, str>,
}

impl<S> FromRequest<S> for TebexWebhookPayload
where
	TebexWebhookState: FromRef<S>,
	String: FromRequest<S>,
	S: Send + Sync,
{
	type Rejection = Response;

	#[tracing::instrument(name = "parse_tebex_webhook_req", level = "debug", skip_all)]
	async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
		let (mut parts, body) = req.into_parts();

		#[cfg(feature = "validate-source-ip")]
		{
			use axum_client_ip::ClientIp;

			let ClientIp(ip) = ClientIp::from_request_parts(&mut parts, state)
				.await
				.map_err(IntoResponse::into_response)?;

			if !super::WEBHOOK_SOURCE_IPS
				.iter()
				.any(|allowed_ip| allowed_ip == &ip)
			{
				return Err(errors::INCORRECT_SOURCE_IP.into_response());
			};
		}

		let signature = parts
			.headers
			.get("X-Signature")
			.ok_or(errors::MISSING_SIGNATURE_HEADER)
			.map_err(IntoResponse::into_response)?
			.to_str()
			.map_err(|_| errors::INVALID_SIGNATURE_HEADER)
			.map_err(IntoResponse::into_response)?
			.to_owned();

		let req = Request::from_parts(parts, body);
		let body = String::from_request(req, state)
			.await
			.map_err(IntoResponse::into_response)?;

		let payload = Self::validate_str(
			&body,
			&signature,
			&TebexWebhookState::from_ref(state).secret,
		)
		.map_err(IntoResponse::into_response)?;

		Ok(payload)
	}
}
