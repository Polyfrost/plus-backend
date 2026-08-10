use aide::{
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State};
use schemars::JsonSchema;
use serde::Serialize;

use crate::api::ApiState;

#[derive(Debug, Serialize, JsonSchema)]
struct DiscoveryDocument {
	issuer: String,
	authorization_endpoint: String,
	token_endpoint: String,
	userinfo_endpoint: String,
	jwks_uri: String,
	response_types_supported: Vec<String>,
	grant_types_supported: Vec<String>,
	subject_types_supported: Vec<String>,
	id_token_signing_alg_values_supported: Vec<String>,
	code_challenge_methods_supported: Vec<String>,
	scopes_supported: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct Jwk {
	kty: &'static str,
	#[serde(rename = "use")]
	usage: &'static str,
	alg: &'static str,
	kid: String,
	n: String,
	e: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct JwksDocument {
	keys: Vec<Jwk>,
}

fn discovery_doc(op: TransformOperation) -> TransformOperation {
	op.id("oidcDiscovery")
		.summary("OpenID Connect discovery document")
		.tag("oidc")
}

fn jwks_doc(op: TransformOperation) -> TransformOperation {
	op.id("oidcJwks")
		.summary("JSON Web Key Set")
		.description("Public keys used to verify tokens issued by this provider.")
		.tag("oidc")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/.well-known/openid-configuration",
			get_with(self::discovery, self::discovery_doc),
		)
		.api_route("/.well-known/jwks.json", get_with(self::jwks, self::jwks_doc))
}

async fn discovery(State(state): State<ApiState>) -> Json<DiscoveryDocument> {
	let issuer = state.oidc_issuer.trim_end_matches('/').to_string();

	Json(DiscoveryDocument {
		authorization_endpoint: format!("{issuer}/oidc/authorize"),
		token_endpoint: format!("{issuer}/oidc/token"),
		userinfo_endpoint: format!("{issuer}/oidc/userinfo"),
		jwks_uri: format!("{issuer}/.well-known/jwks.json"),
		issuer,
		response_types_supported: vec!["code".to_string()],
		grant_types_supported: vec!["authorization_code".to_string()],
		subject_types_supported: vec!["public".to_string()],
		id_token_signing_alg_values_supported: vec!["RS256".to_string()],
		code_challenge_methods_supported: vec!["S256".to_string()],
		scopes_supported: vec!["openid".to_string()],
	})
}

async fn jwks(State(state): State<ApiState>) -> Json<JwksDocument> {
	let key = &state.oidc_signing_key;
	Json(JwksDocument {
		keys: vec![Jwk {
			kty: "RSA",
			usage: "sig",
			alg: "RS256",
			kid: key.kid.clone(),
			n: key.n.clone(),
			e: key.e.clone(),
		}],
	})
}
