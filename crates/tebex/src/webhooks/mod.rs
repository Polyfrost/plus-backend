#[cfg(feature = "axum")]
pub mod axum;
#[cfg(test)]
mod tests;
mod types;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
pub use types::*;

const SHA256_BYTES: usize = 256 / 8;
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum WebhookValidationError {
	#[error("provided signature string was incorrectly formatted as hex: {0}")]
	InvalidSignatureFormat(#[from] base16ct::Error),
	#[error("validation with HMAC failed: {0}")]
	Validation(#[from] hmac::digest::MacError),
	#[error("parsing JSON webhook payload failed: {0}")]
	Parsing(#[from] serde_json::Error)
}

impl TebexWebhookPayload {
	/// Validates and parses a webhook payload from Tebex
	#[tracing::instrument(level = "debug", skip(s))]
	pub fn validate_str(
		s: &str,
		signature: &str,
		secret: &str
	) -> Result<Self, WebhookValidationError> {
		// Validate signature with HMAC
		let webhook_hash = Sha256::digest(s);
		let mut webhook_hash_hex = [0u8; SHA256_BYTES * 2];
		base16ct::lower::encode(&webhook_hash, &mut webhook_hash_hex)
			.expect("infailible: correct size buffer is provided for base16 (bytes * 2)");

		let mut decoded_signature = [0u8; SHA256_BYTES];
		base16ct::lower::decode(signature, &mut decoded_signature)?;

		let mut hmac = HmacSha256::new_from_slice(secret.as_bytes())
			.expect("infailible: HMAC takes any key length");
		hmac.update(&webhook_hash_hex);

		hmac.verify_slice(&decoded_signature)?;

		// If HMAC validation succeeded, parse the actual data
		let parsed = Self::parse_str(s)?;

		Ok(parsed)
	}

	/// Parse a webhook payload using serde, and return the deserialized data
	/// structure. You should probably not use this, but instead use
	/// [TebexWebhook::validate_str] in order to ensure the webhook
	/// correctly originated from Tebex.
	pub fn parse_str(s: &str) -> Result<Self, serde_json::Error> {
		serde_json::from_str(s)
	}
}
