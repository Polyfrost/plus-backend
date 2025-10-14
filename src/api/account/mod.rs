mod login;

use aide::{OperationInput, axum::ApiRouter, openapi::SecurityRequirement};
use axum::{
	extract::FromRequestParts,
	http::request::Parts,
	response::{IntoResponse, Response}
};
use http::StatusCode;
use pasetors::{
	Local,
	claims::ClaimsValidationRules,
	local,
	token::UntrustedToken,
	version4::V4
};
use reqwest::header::AUTHORIZATION;
use uuid::Uuid;

use crate::api::ApiState;

pub const OPENAPI_SECURITY_NAME: &str = "Bearer Token";
pub const PASETO_IMPLICIT_ASSERT: Option<&[u8]> = Some(b"plus-backend");

pub struct AuthenticationExtractor(pub Uuid);

impl OperationInput for AuthenticationExtractor {
	fn operation_input(
		_ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation
	) {
		operation.security.push(SecurityRequirement::from([(
			OPENAPI_SECURITY_NAME.to_string(),
			Vec::new()
		)]));
	}
}

const MISSING_AUTHORIZATION_ERR: (StatusCode, &str) =
	(StatusCode::UNAUTHORIZED, "Authorization header was missing");
const INVALID_AUTHORIZATION_ERR: (StatusCode, &str) =
	(StatusCode::UNAUTHORIZED, "Authorization header was invalid");

impl FromRequestParts<ApiState> for AuthenticationExtractor {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState
	) -> Result<Self, Self::Rejection> {
		let header = parts
			.headers
			.get(AUTHORIZATION)
			.ok_or(MISSING_AUTHORIZATION_ERR)
			.map_err(IntoResponse::into_response)?
			.to_str()
			.map_err(|_| INVALID_AUTHORIZATION_ERR.into_response())?;

		let Some(token) = header.strip_prefix("Bearer ") else {
			return Err(INVALID_AUTHORIZATION_ERR.into_response());
		};

		let token = local::decrypt(
			&state.paseto_key,
			&UntrustedToken::<Local, V4>::try_from(token)
				.map_err(|_| INVALID_AUTHORIZATION_ERR.into_response())?,
			&ClaimsValidationRules::new(),
			None,
			PASETO_IMPLICIT_ASSERT
		)
		.map_err(|_| INVALID_AUTHORIZATION_ERR.into_response())?;
		let claims = token
			.payload_claims()
			.ok_or(MISSING_AUTHORIZATION_ERR.into_response())?;

		let sub = claims
			.get_claim("sub")
			.ok_or(MISSING_AUTHORIZATION_ERR.into_response())?
			.as_str()
			.ok_or(MISSING_AUTHORIZATION_ERR.into_response())?;

		Uuid::parse_str(sub)
			.map(Self)
			.map_err(|_| MISSING_AUTHORIZATION_ERR.into_response())
	}
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().merge(login::router())
}
