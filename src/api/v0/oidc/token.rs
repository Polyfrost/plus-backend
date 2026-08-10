use aide::{
	OperationIo,
	axum::{ApiRouter, routing::{get_with, post_with}},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::{StatusCode, header::AUTHORIZATION},
	response::IntoResponse,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use jsonwebtoken::{Algorithm, Header, Validation, decode, encode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::{ApiState, v0::oidc::TOKEN_TTL_SECS};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum TokenError {
	#[error("Unsupported grant_type, only authorization_code is accepted")]
	UnsupportedGrant,
	#[error("The authorization code is invalid, expired, or already used")]
	InvalidCode,
	#[error("client_id or redirect_uri did not match the authorization request")]
	Mismatch,
	#[error("PKCE verification failed")]
	PkceFailed,
	#[error("Unable to sign token: {0}")]
	Signing(#[from] jsonwebtoken::errors::Error),
	#[error("Missing or invalid Authorization header")]
	Unauthorized,
}

impl IntoResponse for TokenError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::UnsupportedGrant | Self::InvalidCode | Self::Mismatch | Self::PkceFailed => {
					StatusCode::BAD_REQUEST
				}
				Self::Unauthorized => StatusCode::UNAUTHORIZED,
				Self::Signing(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TokenRequest {
	grant_type: String,
	code: String,
	client_id: String,
	redirect_uri: String,
	code_verifier: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TokenResponse {
	access_token: String,
	id_token: String,
	token_type: &'static str,
	expires_in: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
	iss: String,
	sub: String,
	nickname: String,
	aud: String,
	exp: i64,
	iat: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct UserinfoResponse {
	sub: String,
	nickname: String,
}

fn token_doc(op: TransformOperation) -> TransformOperation {
	op.id("oidcToken")
		.summary("Exchange an authorization code for tokens")
		.description(
			"Redeems a code minted by /oidc/authorize (one-time use) for a \
			 signed RS256 id_token/access_token pair, after verifying the PKCE \
			 code_verifier.",
		)
		.tag("oidc")
}

fn userinfo_doc(op: TransformOperation) -> TransformOperation {
	op.id("oidcUserinfo")
		.summary("Resolve the subject of a token issued by this provider")
		.tag("oidc")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/oidc/token", post_with(self::token, self::token_doc))
		.api_route("/oidc/userinfo", get_with(self::userinfo, self::userinfo_doc))
}

fn pkce_matches(verifier: &str, challenge: &str) -> bool {
	let digest = Sha256::digest(verifier.as_bytes());
	URL_SAFE_NO_PAD.encode(digest) == challenge
}

#[tracing::instrument(level = "debug", skip(state, body), fields(client_id = %body.client_id))]
async fn token(
	State(state): State<ApiState>,
	Json(body): Json<TokenRequest>,
) -> Result<Json<TokenResponse>, TokenError> {
	if body.grant_type != "authorization_code" {
		return Err(TokenError::UnsupportedGrant);
	}

	let Some(authorization) = state.oidc_codes.remove(&body.code).await else {
		return Err(TokenError::InvalidCode);
	};

	if authorization.client_id != body.client_id || authorization.redirect_uri != body.redirect_uri
	{
		return Err(TokenError::Mismatch);
	}
	if !pkce_matches(&body.code_verifier, &authorization.code_challenge) {
		return Err(TokenError::PkceFailed);
	}

	let now = Utc::now().timestamp();
	let claims = Claims {
		iss: state.oidc_issuer.trim_end_matches('/').to_string(),
		sub: authorization.minecraft_uuid.to_string(),
		nickname: authorization.minecraft_uuid.to_string(),
		aud: authorization.client_id,
		exp: now + TOKEN_TTL_SECS,
		iat: now,
	};

	let mut header = Header::new(Algorithm::RS256);
	header.kid = Some(state.oidc_signing_key.kid.clone());
	let token = encode(&header, &claims, &state.oidc_signing_key.encoding_key)?;

	Ok(Json(TokenResponse {
		access_token: token.clone(),
		id_token: token,
		token_type: "Bearer",
		expires_in: TOKEN_TTL_SECS,
	}))
}

#[tracing::instrument(level = "debug", skip(state, headers))]
async fn userinfo(
	State(state): State<ApiState>,
	headers: axum::http::HeaderMap,
) -> Result<Json<UserinfoResponse>, TokenError> {
	let header = headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.ok_or(TokenError::Unauthorized)?;
	let token = header.strip_prefix("Bearer ").ok_or(TokenError::Unauthorized)?;

	let mut validation = Validation::new(Algorithm::RS256);
	validation.validate_aud = false;
	validation.set_issuer(&[state.oidc_issuer.trim_end_matches('/')]);

	let data = decode::<Claims>(token, &state.oidc_signing_key.decoding_key, &validation)
		.map_err(|_| TokenError::Unauthorized)?;

	Ok(Json(UserinfoResponse {
		sub: data.claims.sub,
		nickname: data.claims.nickname,
	}))
}
