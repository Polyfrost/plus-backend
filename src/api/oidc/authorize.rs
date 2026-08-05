use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::{ApiState, account::AuthenticatedPlayer, oidc::AuthorizationCode};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum AuthorizeError {
	#[error("Only the S256 PKCE code challenge method is supported")]
	UnsupportedChallengeMethod,
}

impl IntoResponse for AuthorizeError {
	fn into_response(self) -> axum::response::Response {
		(StatusCode::BAD_REQUEST, self.to_string()).into_response()
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AuthorizeRequest {
	client_id: String,
	redirect_uri: String,
	/// PKCE code challenge, computed as BASE64URL(SHA256(code_verifier)).
	code_challenge: String,
	/// Must be "S256"; plaintext ("plain") challenges are not supported.
	code_challenge_method: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AuthorizeResponse {
	code: String,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("oidcAuthorize")
		.summary("Mint an OIDC authorization code")
		.description(
			"Exchanges the caller's existing Poly+ session for a short-lived \
			 PKCE authorization code, redeemable once at /oidc/token. Used by \
			 the Poly+ mod to bridge an authenticated session into an EOS \
			 Connect login via OpenIdAccessToken.",
		)
		.tag("oidc")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/oidc/authorize", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<AuthorizeRequest>,
) -> Result<Json<AuthorizeResponse>, AuthorizeError> {
	if body.code_challenge_method != "S256" {
		return Err(AuthorizeError::UnsupportedChallengeMethod);
	}

	let mut code_bytes = [0u8; 32];
	rand::rng().fill_bytes(&mut code_bytes);
	let code = URL_SAFE_NO_PAD.encode(code_bytes);

	state
		.oidc_codes
		.insert(
			code.clone(),
			AuthorizationCode {
				minecraft_uuid: player.minecraft_uuid,
				client_id: body.client_id,
				redirect_uri: body.redirect_uri,
				code_challenge: body.code_challenge,
			},
		)
		.await;

	Ok(Json(AuthorizeResponse { code }))
}
