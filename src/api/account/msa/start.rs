use aide::{
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation
};
use axum::{extract::State, response::Redirect};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use rand::{TryRngCore, rngs::OsRng};
use sha2::{Digest, Sha256};

use crate::api::{ApiState, account::msa::MsaError};

const MSA_AUTHORIZE_URL: &str =
	"https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("start")
		.summary("Start Microsoft login")
		.description("Redirects to Microsoft OAuth2 authorize URL (PKCE).")
		.tag("account")
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(State(state): State<ApiState>) -> Result<Redirect, MsaError> {
	let oauth_state = random_urlsafe(24);
	let code_verifier = random_urlsafe(32);
	let code_challenge = pkce_challenge_s256(&code_verifier);

	state
		.msa
		.pkce_cache
		.insert(oauth_state.clone(), code_verifier)
		.await;

	// NOTE: We request User.Read so we can call Graph /me for a stable id.
	let scope = "openid profile email User.Read";

	let authorize_url = format!(
		"{base}?client_id={client_id}&response_type=code&redirect_uri={redirect_uri}&\
		 response_mode=query&scope={scope}&state={oauth_state}&\
		 code_challenge={code_challenge}&code_challenge_method=S256",
		base = MSA_AUTHORIZE_URL,
		client_id = urlencoding::encode(&state.msa.client_id),
		redirect_uri = urlencoding::encode(&state.msa.redirect_uri),
		scope = urlencoding::encode(scope),
		oauth_state = urlencoding::encode(&oauth_state),
		code_challenge = urlencoding::encode(&code_challenge),
	);

	Ok(Redirect::temporary(&authorize_url))
}

fn random_urlsafe(bytes: usize) -> String {
	let mut buf = vec![0u8; bytes];
	OsRng.try_fill_bytes(&mut buf);
	BASE64_URL_SAFE_NO_PAD.encode(buf)
}

fn pkce_challenge_s256(verifier: &str) -> String {
	let hash = Sha256::digest(verifier.as_bytes());
	BASE64_URL_SAFE_NO_PAD.encode(hash)
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/start", get_with(self::endpoint, self::endpoint_doc))
}
