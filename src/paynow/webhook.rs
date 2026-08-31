use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use http::HeaderMap;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub(crate) const TIMESTAMP_HEADER: &str = "paynow-timestamp";
pub(crate) const SIGNATURE_HEADER: &str = "paynow-signature";

/// PayNow's documented replay window.
const TOLERANCE_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WebhookError {
	#[error("missing {TIMESTAMP_HEADER} header")]
	MissingTimestamp,
	#[error("missing {SIGNATURE_HEADER} header")]
	MissingSignature,
	#[error("malformed {TIMESTAMP_HEADER} header")]
	BadTimestamp,
	#[error("signature is not valid base64")]
	BadEncoding,
	#[error("timestamp is outside the accepted window")]
	Stale,
	#[error("signature mismatch")]
	Mismatch,
}

/// `body` must be the raw request bytes: re-serialising the JSON changes
/// what was signed.
pub(crate) fn verify(
	secret: &[u8],
	headers: &HeaderMap,
	body: &[u8],
	now_ms: i64,
) -> Result<(), WebhookError> {
	let timestamp = headers
		.get(TIMESTAMP_HEADER)
		.and_then(|value| value.to_str().ok())
		.ok_or(WebhookError::MissingTimestamp)?
		.trim();
	let signature = headers
		.get(SIGNATURE_HEADER)
		.and_then(|value| value.to_str().ok())
		.ok_or(WebhookError::MissingSignature)?
		.trim();

	let sent_ms: i64 = timestamp.parse().map_err(|_| WebhookError::BadTimestamp)?;
	if (now_ms - sent_ms).abs() > TOLERANCE_MS {
		return Err(WebhookError::Stale);
	}

	let expected = sign(secret, timestamp, body);

	// The header may carry several digests during a secret rotation.
	let mut seen = false;
	for candidate in signature.split(',') {
		let candidate = candidate.trim();
		if candidate.is_empty() {
			continue;
		}
		seen = true;

		let Ok(provided) = STANDARD.decode(candidate) else {
			continue;
		};
		// Constant time, unlike comparing the encoded strings.
		if expected.clone().verify_slice(&provided).is_ok() {
			return Ok(());
		}
	}

	if seen {
		Err(WebhookError::Mismatch)
	} else {
		Err(WebhookError::BadEncoding)
	}
}

fn sign(secret: &[u8], timestamp: &str, body: &[u8]) -> HmacSha256 {
	let mut mac =
		HmacSha256::new_from_slice(secret).expect("hmac accepts a key of any length");
	mac.update(timestamp.as_bytes());
	mac.update(b".");
	mac.update(body);
	mac
}

#[cfg(test)]
mod tests {
	use super::*;

	const SECRET: &[u8] = b"whsec_testing";
	const BODY: &[u8] = br#"{"event_type":"ON_ORDER_COMPLETED"}"#;
	const NOW: i64 = 1_777_000_000_000;

	fn signature(timestamp: &str, body: &[u8]) -> String {
		STANDARD.encode(sign(SECRET, timestamp, body).finalize().into_bytes())
	}

	fn headers(timestamp: Option<&str>, signature: Option<&str>) -> HeaderMap {
		let mut headers = HeaderMap::new();
		if let Some(timestamp) = timestamp {
			headers.insert(
				TIMESTAMP_HEADER,
				timestamp
					.parse()
					.expect("timestamp is a valid header value"),
			);
		}
		if let Some(signature) = signature {
			headers.insert(
				SIGNATURE_HEADER,
				signature
					.parse()
					.expect("signature is a valid header value"),
			);
		}
		headers
	}

	#[test]
	fn accepts_a_valid_signature() {
		let timestamp = NOW.to_string();
		let headers = headers(Some(&timestamp), Some(&signature(&timestamp, BODY)));
		assert!(verify(SECRET, &headers, BODY, NOW).is_ok());
	}

	#[test]
	fn accepts_one_of_several_signatures() {
		let timestamp = NOW.to_string();
		let combined = format!(
			"{}, {}",
			signature(&timestamp, b"other"),
			signature(&timestamp, BODY)
		);
		let headers = headers(Some(&timestamp), Some(&combined));
		assert!(verify(SECRET, &headers, BODY, NOW).is_ok());
	}

	#[test]
	fn rejects_a_tampered_body() {
		let timestamp = NOW.to_string();
		let headers = headers(Some(&timestamp), Some(&signature(&timestamp, BODY)));
		let tampered = br#"{"event_type":"ON_ORDER_CANCELLED"}"#;
		assert!(matches!(
			verify(SECRET, &headers, tampered, NOW),
			Err(WebhookError::Mismatch)
		));
	}

	#[test]
	fn rejects_another_secret() {
		let timestamp = NOW.to_string();
		let headers = headers(Some(&timestamp), Some(&signature(&timestamp, BODY)));
		assert!(matches!(
			verify(b"whsec_other", &headers, BODY, NOW),
			Err(WebhookError::Mismatch)
		));
	}

	#[test]
	fn rejects_a_stale_timestamp() {
		let timestamp = (NOW - 6 * 60 * 1000).to_string();
		let headers = headers(Some(&timestamp), Some(&signature(&timestamp, BODY)));
		assert!(matches!(
			verify(SECRET, &headers, BODY, NOW),
			Err(WebhookError::Stale)
		));
	}

	#[test]
	fn rejects_a_timestamp_from_the_future() {
		let timestamp = (NOW + 6 * 60 * 1000).to_string();
		let headers = headers(Some(&timestamp), Some(&signature(&timestamp, BODY)));
		assert!(matches!(
			verify(SECRET, &headers, BODY, NOW),
			Err(WebhookError::Stale)
		));
	}

	#[test]
	fn rejects_a_malformed_timestamp() {
		let headers = headers(Some("not-a-number"), Some("irrelevant"));
		assert!(matches!(
			verify(SECRET, &headers, BODY, NOW),
			Err(WebhookError::BadTimestamp)
		));
	}

	#[test]
	fn rejects_a_signature_that_is_not_base64() {
		let timestamp = NOW.to_string();
		let headers = headers(Some(&timestamp), Some("!!!not base64!!!"));
		assert!(matches!(
			verify(SECRET, &headers, BODY, NOW),
			Err(WebhookError::Mismatch)
		));
	}

	#[test]
	fn rejects_missing_headers() {
		let timestamp = NOW.to_string();
		assert!(matches!(
			verify(SECRET, &headers(None, Some("x")), BODY, NOW),
			Err(WebhookError::MissingTimestamp)
		));
		assert!(matches!(
			verify(SECRET, &headers(Some(&timestamp), None), BODY, NOW),
			Err(WebhookError::MissingSignature)
		));
	}
}
