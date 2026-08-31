use std::{net::IpAddr, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use http::{HeaderMap, StatusCode, header};
use rand::Rng as _;
use reqwest::{Client, ClientBuilder, Method, RequestBuilder, Response};
use serde::{Serialize, de::DeserializeOwned};
use tracing::warn;

use super::models::ApiErrorBody;

pub(crate) const DEFAULT_API_BASE: &str = "https://api.paynow.gg";

const USER_AGENT: &str = concat!("plus-backend/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ATTEMPTS: u32 = 3;
const BODY_LOG_LIMIT: usize = 512;
/// However long PayNow asks us to wait, we will not hold a request open for
/// longer than this before giving up on it.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub(crate) enum PayNowError {
	#[error("Unable to reach PayNow: {0}")]
	Transport(#[from] reqwest::Error),
	#[error("PayNow returned {status}{}: {message}", .code.as_deref().map(|c| format!(" ({c})")).unwrap_or_default())]
	Api {
		status: StatusCode,
		code: Option<String>,
		message: String,
	},
	#[error("Unable to decode PayNow response: {0}")]
	Decode(#[source] serde_json::Error),
}

impl PayNowError {
	/// A lookup 404 means "does not exist yet", not a failure.
	pub(crate) fn is_not_found(&self) -> bool {
		matches!(self, Self::Api { status, .. } if *status == StatusCode::NOT_FOUND)
	}

	/// PayNow blaming the request rather than itself, which for a checkout
	/// means the caller sent something a buyer can correct.
	pub(crate) fn is_client_error(&self) -> bool {
		matches!(self, Self::Api { status, .. } if status.is_client_error())
	}

	pub(crate) fn message(&self) -> Option<&str> {
		match self {
			Self::Api { message, .. } => Some(message),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retry {
	Idempotent,
	/// Repeat only when the request provably never left this process: a
	/// duplicate checkout or customer is worse than a failed one.
	ConnectOnly,
}

#[derive(Clone)]
pub(crate) struct PayNowClient {
	http: Client,
	base: Arc<str>,
	store_id: Arc<str>,
	api_key: Arc<str>,
}

impl std::fmt::Debug for PayNowClient {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PayNowClient")
			.field("store_id", &self.store_id)
			.finish_non_exhaustive()
	}
}

impl PayNowClient {
	pub(crate) fn new(base: &str, store_id: &str, api_key: &str) -> Self {
		PayNowClient {
			// Not the shared client: that one has no timeout.
			http: ClientBuilder::new()
				.https_only(base.starts_with("https://"))
				.user_agent(USER_AGENT)
				.timeout(REQUEST_TIMEOUT)
				.connect_timeout(CONNECT_TIMEOUT)
				.build()
				.expect("Unable to build PayNow http client"),
			base: base.trim_end_matches('/').into(),
			store_id: store_id.into(),
			api_key: api_key.into(),
		}
	}

	pub(crate) fn store_id(&self) -> &str {
		&self.store_id
	}

	fn url(&self, suffix: &str) -> String {
		format!("{}/v1/stores/{}{suffix}", self.base, self.store_id)
	}

	fn request(&self, method: Method, url: String) -> RequestBuilder {
		self.http.request(method, url).header(
			http::header::AUTHORIZATION,
			format!("APIKey {}", self.api_key),
		)
	}

	pub(crate) async fn get<T: DeserializeOwned>(
		&self,
		suffix: &str,
	) -> Result<T, PayNowError> {
		let url = self.url(suffix);
		self.send_json(Method::GET, url, None::<&()>, Retry::Idempotent, None)
			.await
	}

	/// For paths outside the store scope, such as the store itself.
	pub(crate) async fn get_unscoped<T: DeserializeOwned>(
		&self,
		path: &str,
	) -> Result<T, PayNowError> {
		let url = format!("{}{path}", self.base);
		self.send_json(Method::GET, url, None::<&()>, Retry::Idempotent, None)
			.await
	}

	pub(crate) async fn post<B: Serialize, T: DeserializeOwned>(
		&self,
		suffix: &str,
		body: &B,
		retry: Retry,
	) -> Result<T, PayNowError> {
		self.post_for(suffix, body, retry, None).await
	}

	/// `customer_ip` is forwarded to PayNow's fraud checks, which cannot see
	/// the buyer directly when the request comes from a server.
	pub(crate) async fn post_for<B: Serialize, T: DeserializeOwned>(
		&self,
		suffix: &str,
		body: &B,
		retry: Retry,
		customer_ip: Option<IpAddr>,
	) -> Result<T, PayNowError> {
		let url = self.url(suffix);
		self.send_json(Method::POST, url, Some(body), retry, customer_ip)
			.await
	}

	pub(crate) async fn patch<B: Serialize, T: DeserializeOwned>(
		&self,
		suffix: &str,
		body: &B,
	) -> Result<T, PayNowError> {
		let url = self.url(suffix);
		self.send_json(Method::PATCH, url, Some(body), Retry::Idempotent, None)
			.await
	}

	/// Forwards a request to a store-scoped path and hands back the raw body,
	/// empty for a 204. Used by the admin proxy, which has no opinion about
	/// the shapes PayNow accepts.
	pub(crate) async fn forward(
		&self,
		method: Method,
		suffix: &str,
		body: Option<&serde_json::Value>,
		retry: Retry,
	) -> Result<Vec<u8>, PayNowError> {
		let url = self.url(suffix);
		self.send(method, url, body, retry, None).await
	}

	async fn send_json<B: Serialize, T: DeserializeOwned>(
		&self,
		method: Method,
		url: String,
		body: Option<&B>,
		retry: Retry,
		customer_ip: Option<IpAddr>,
	) -> Result<T, PayNowError> {
		let bytes = self.send(method, url, body, retry, customer_ip).await?;
		// Not `Response::json`, so an unexpected shape can still be logged.
		serde_json::from_slice(&bytes).map_err(|error| {
			warn!(
				body = %String::from_utf8_lossy(&bytes[..bytes.len().min(BODY_LOG_LIMIT)]),
				"Unable to decode PayNow response"
			);
			PayNowError::Decode(error)
		})
	}

	async fn send<B: Serialize>(
		&self,
		method: Method,
		url: String,
		body: Option<&B>,
		retry: Retry,
		customer_ip: Option<IpAddr>,
	) -> Result<Vec<u8>, PayNowError> {
		let mut attempt = 0;
		loop {
			attempt += 1;

			let mut request = self.request(method.clone(), url.clone());
			if let Some(ip) = customer_ip {
				request = request.header("x-paynow-customer-ip", ip.to_string());
			}
			if let Some(body) = body {
				request = request.json(body);
			}

			// The headers are read before the body, which consumes the response.
			let (error, retry_after) = match request.send().await {
				Ok(response) => {
					let retry_after = retry_after(response.headers(), Utc::now);
					match Self::decode(response).await {
						Ok(bytes) => return Ok(bytes),
						Err(error) => (error, retry_after),
					}
				}
				Err(error) => (PayNowError::Transport(error), None),
			};

			if attempt >= MAX_ATTEMPTS || !should_retry(&error, retry) {
				return Err(error);
			}

			let delay = retry_after.unwrap_or_else(|| backoff(attempt));
			warn!(
				%url,
				attempt,
				delay_ms = delay.as_millis(),
				"Retrying PayNow request: {error}"
			);
			tokio::time::sleep(delay).await;
		}
	}

	async fn decode(response: Response) -> Result<Vec<u8>, PayNowError> {
		let status = response.status();
		let bytes = response.bytes().await?.to_vec();
		if status.is_success() {
			return Ok(bytes);
		}

		let parsed = serde_json::from_slice::<ApiErrorBody>(&bytes).unwrap_or_default();
		Err(PayNowError::Api {
			status,
			code: parsed.code,
			message: parsed.message.unwrap_or_else(|| {
				String::from_utf8_lossy(&bytes[..bytes.len().min(BODY_LOG_LIMIT)])
					.into_owned()
			}),
		})
	}
}

fn should_retry(error: &PayNowError, retry: Retry) -> bool {
	match error {
		PayNowError::Transport(error) => retry == Retry::Idempotent || error.is_connect(),
		PayNowError::Api { status, .. } => {
			retry == Retry::Idempotent
				&& (status.is_server_error()
					|| *status == StatusCode::REQUEST_TIMEOUT
					|| *status == StatusCode::TOO_MANY_REQUESTS)
		}
		PayNowError::Decode(_) => false,
	}
}

/// 200ms, 400ms, 800ms, jittered so concurrent retries spread out.
fn backoff(attempt: u32) -> Duration {
	let base = 200u64 << (attempt.saturating_sub(1)).min(4);
	let jitter = rand::rng().random_range(0.75..1.25);
	Duration::from_millis((base as f64 * jitter) as u64)
}

/// `Retry-After` as either delta-seconds or an HTTP date, capped so a long
/// wait becomes a failure rather than a stalled request. `None` falls back to
/// the fixed backoff.
fn retry_after(headers: &HeaderMap, now: impl Fn() -> DateTime<Utc>) -> Option<Duration> {
	let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();

	let seconds = match value.parse::<u64>() {
		Ok(seconds) => Duration::from_secs(seconds),
		Err(_) => {
			let until = DateTime::parse_from_rfc2822(value)
				.ok()?
				.with_timezone(&Utc);
			(until - now()).to_std().unwrap_or(Duration::ZERO)
		}
	};

	Some(seconds.min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn headers(value: &str) -> HeaderMap {
		let mut headers = HeaderMap::new();
		headers.insert(
			header::RETRY_AFTER,
			value.parse().expect("value is a valid header"),
		);
		headers
	}

	fn at(rfc2822: &str) -> DateTime<Utc> {
		DateTime::parse_from_rfc2822(rfc2822)
			.expect("fixture is a valid date")
			.with_timezone(&Utc)
	}

	#[test]
	fn reads_delta_seconds() {
		assert_eq!(
			retry_after(&headers("5"), Utc::now),
			Some(Duration::from_secs(5))
		);
	}

	#[test]
	fn reads_an_http_date() {
		let now = || at("Wed, 26 Aug 2026 10:00:00 +0000");
		assert_eq!(
			retry_after(&headers("Wed, 26 Aug 2026 10:00:12 +0000"), now),
			Some(Duration::from_secs(12))
		);
	}

	#[test]
	fn a_date_in_the_past_waits_no_time() {
		let now = || at("Wed, 26 Aug 2026 10:00:00 +0000");
		assert_eq!(
			retry_after(&headers("Wed, 26 Aug 2026 09:59:00 +0000"), now),
			Some(Duration::ZERO)
		);
	}

	#[test]
	fn a_long_wait_is_capped() {
		assert_eq!(
			retry_after(&headers("86400"), Utc::now),
			Some(MAX_RETRY_AFTER)
		);
	}

	#[test]
	fn falls_back_when_absent_or_unreadable() {
		assert_eq!(retry_after(&HeaderMap::new(), Utc::now), None);
		assert_eq!(retry_after(&headers("soon"), Utc::now), None);
	}
}
