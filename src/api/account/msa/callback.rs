use std::time::Duration;

use aide::{
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation
};
use axum::{
	Json,
	extract::{Query, State}
};
use pasetors::{claims::Claims, local};
use schemars::JsonSchema;
use serde::Deserialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	account::{PASETO_IMPLICIT_ASSERT, msa::MsaError}
};

const MSA_TOKEN_URL: &str =
	"https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const GRAPH_ME_URL: &str = "https://graph.microsoft.com/v1.0/me";

// Namespace for UUID v5 derived from Microsoft account IDs.
const MSA_UUID_NAMESPACE: Uuid = Uuid::from_bytes([
	0x12, 0x7b, 0x2a, 0x8b, 0x2a, 0x2e, 0x4f, 0x3f, 0x90, 0xb6, 0x0a, 0x93, 0x58, 0x9a,
	0x71, 0x2c
]);

#[derive(Debug, Deserialize, JsonSchema)]
struct CallbackQuery {
	code: String,
	state: String
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
	access_token: String /* id_token: Option<String>,
	                      * refresh_token: Option<String>,
	                      * expires_in: Option<i64>,
	                      * token_type: Option<String>, */
}

#[derive(Debug, Deserialize)]
struct GraphMeResponse {
	// This is the stable AAD/consumers user id you can key off.
	id: String // displayName/mail/etc available if you want
}

#[derive(Debug, Default, serde::Serialize, JsonSchema)]
pub struct LoginResponse {
	token: String
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("callback")
		.summary("Complete Microsoft login")
		.description("Exchanges code for tokens, fetches /me, mints PASETO.")
		.tag("account")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/callback", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	Query(query): Query<CallbackQuery>
) -> Result<Json<LoginResponse>, MsaError> {
	let Some(code_verifier) = state.msa.pkce_cache.get(&query.state).await else {
		return Err(MsaError::InvalidState);
	};

	// Exchange code -> tokens
	let token_res = state
		.client
		.post(MSA_TOKEN_URL)
		.form(&[
			("client_id", state.msa.client_id.as_str()),
			("client_secret", state.msa.client_secret.as_str()),
			("grant_type", "authorization_code"),
			("code", query.code.as_str()),
			("redirect_uri", state.msa.redirect_uri.as_str()),
			("code_verifier", code_verifier.as_str())
		])
		.send()
		.await
		.map_err(|_| MsaError::TokenExchange)?
		.error_for_status()
		.map_err(|_| MsaError::TokenExchange)?;

	let token_body: TokenResponse = token_res
		.json()
		.await
		.map_err(|_| MsaError::TokenExchange)?;

	// Fetch identity
	let me_res = state
		.client
		.get(GRAPH_ME_URL)
		.bearer_auth(&token_body.access_token)
		.send()
		.await
		.map_err(|_| MsaError::GraphFailed)?
		.error_for_status()
		.map_err(|_| MsaError::GraphFailed)?;

	let me: GraphMeResponse = me_res.json().await.map_err(|_| MsaError::GraphFailed)?;

	// Keep your current extractor unchanged by deriving a UUID from Microsoft id.
	let user_uuid = Uuid::new_v5(&MSA_UUID_NAMESPACE, me.id.as_bytes());

	// Mint your existing PASETO
	let token = local::encrypt(
		&state.paseto_key,
		&{
			let mut claims = Claims::new().map_err(|_| MsaError::TokenMint)?;
			claims
				.set_expires_in(&Duration::from_secs(60 * 60 * 2))
				.map_err(|_| MsaError::TokenMint)?;
			claims
				.subject(&user_uuid.as_hyphenated().to_string())
				.map_err(|_| MsaError::TokenMint)?;
			claims
		},
		None,
		PASETO_IMPLICIT_ASSERT
	)
	.map_err(|_| MsaError::TokenMint)?;

	Ok(Json(LoginResponse { token }))
}
