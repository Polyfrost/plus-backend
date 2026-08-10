mod authorize;
mod discovery;
mod token;

use std::time::Duration;

use aide::axum::ApiRouter;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{DecodingKey, EncodingKey};
use moka::future::Cache;
use rsa::{
	RsaPrivateKey, RsaPublicKey,
	pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey},
	traits::PublicKeyParts,
};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use crate::api::ApiState;

const AUTH_CODE_TTL: Duration = Duration::from_secs(5 * 60);
const TOKEN_TTL_SECS: i64 = 10 * 60;

#[derive(Clone)]
pub(crate) struct OidcSigningKey {
	pub(crate) kid: String,
	pub(crate) encoding_key: EncodingKey,
	pub(crate) decoding_key: DecodingKey,
	pub(crate) n: String,
	pub(crate) e: String,
}

impl std::fmt::Debug for OidcSigningKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("OidcSigningKey")
			.field("kid", &self.kid)
			.finish_non_exhaustive()
	}
}

impl OidcSigningKey {
	fn from_private_key(kid: String, private_key: &RsaPrivateKey) -> Self {
		let der = private_key
			.to_pkcs1_der()
			.expect("Unable to encode RSA private key as PKCS#1 DER");
		let encoding_key = EncodingKey::from_rsa_der(der.as_bytes());

		let public_key = RsaPublicKey::from(private_key);
		let n_bytes = public_key.n().to_bytes_be();
		let e_bytes = public_key.e().to_bytes_be();
		let decoding_key = DecodingKey::from_rsa_raw_components(&n_bytes, &e_bytes);

		Self {
			kid,
			encoding_key,
			decoding_key,
			n: URL_SAFE_NO_PAD.encode(n_bytes),
			e: URL_SAFE_NO_PAD.encode(e_bytes),
		}
	}
}

pub(crate) async fn load_or_generate_signing_key(db: &DatabaseConnection) -> OidcSigningKey {
	use entities::{oidc_signing_keys, prelude::*};
	use sea_orm::{ActiveValue, EntityTrait, Set};

	if let Some(existing) = OidcSigningKeys::find()
		.one(db)
		.await
		.expect("Unable to query OIDC signing keys")
	{
		let private_key = RsaPrivateKey::from_pkcs1_der(&existing.private_key_der)
			.expect("Stored OIDC signing key is corrupt");
		return OidcSigningKey::from_private_key(existing.kid, &private_key);
	}

	let private_key = RsaPrivateKey::new(&mut rand_core::OsRng, 2048)
		.expect("Unable to generate RSA keypair for OIDC signing");
	let kid = Uuid::new_v4().to_string();
	let der = private_key
		.to_pkcs1_der()
		.expect("Unable to encode RSA private key as PKCS#1 DER");

	OidcSigningKeys::insert(oidc_signing_keys::ActiveModel {
		kid: Set(kid.clone()),
		private_key_der: Set(der.as_bytes().to_vec()),
		created_at: ActiveValue::NotSet,
	})
	.exec_without_returning(db)
	.await
	.expect("Unable to persist OIDC signing key");

	OidcSigningKey::from_private_key(kid, &private_key)
}

pub(crate) fn new_authorization_code_cache() -> Cache<String, AuthorizationCode> {
	Cache::builder().time_to_live(AUTH_CODE_TTL).build()
}

#[derive(Debug, Clone)]
pub(crate) struct AuthorizationCode {
	pub(crate) minecraft_uuid: Uuid,
	pub(crate) client_id: String,
	pub(crate) redirect_uri: String,
	pub(crate) code_challenge: String,
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(discovery::router())
		.merge(authorize::router())
		.merge(token::router())
}
