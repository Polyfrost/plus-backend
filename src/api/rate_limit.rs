use std::{
	hash::Hash,
	net::{IpAddr, Ipv6Addr},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
	extract::{Request, State},
	http::{StatusCode, header},
	middleware::Next,
	response::{IntoResponse, Response},
};
use axum_client_ip::ClientIp;
use moka::future::Cache;

use crate::api::api_tokens::{ApiTokens, TOKEN_HEADER};

// TODO: Redis?
#[derive(Debug, Clone)]
pub(in crate::api) struct RateLimiter<K: Hash + Eq + Send + Sync + 'static> {
	hits: Cache<(K, u64), u32>,
	window: Duration,
	limit: u32,
}

impl<K: Hash + Eq + Send + Sync + 'static> RateLimiter<K> {
	pub(in crate::api) fn new(limit: u32, window: Duration) -> Self {
		assert!(
			window.as_secs() > 0,
			"the rate limit window must be whole seconds"
		);

		RateLimiter {
			// Two windows, so the window being filled never evicts itself.
			hits: Cache::builder().time_to_live(window * 2).build(),
			window,
			limit,
		}
	}

	/// Records a hit, returning how long to wait once the key is over its
	/// limit.
	pub(in crate::api) async fn check(&self, key: K) -> Option<Duration> {
		let window = self.window.as_secs();
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_secs();

		let hits = self
			.hits
			.entry((key, now / window))
			.and_upsert_with(|entry| async move {
				entry.map_or(1, |entry| entry.into_value().saturating_add(1))
			})
			.await
			.into_value();

		(hits > self.limit).then(|| Duration::from_secs(window - now % window))
	}
}

/// IPv6 clients usually hold a whole /64, so it is limited as one address.
pub(in crate::api) fn address_key(ip: IpAddr) -> IpAddr {
	match ip.to_canonical() {
		IpAddr::V6(ip) => {
			IpAddr::V6(Ipv6Addr::from_bits(ip.to_bits() & !u128::from(u64::MAX)))
		}
		ip => ip,
	}
}

/// The per-address limits, picked per request by path.
#[derive(Debug, Clone)]
pub(in crate::api) struct RequestLimits {
	pub(in crate::api) default: RateLimiter<IpAddr>,
	pub(in crate::api) assets: RateLimiter<IpAddr>,
}

impl RequestLimits {
	fn for_path(&self, path: &str) -> &RateLimiter<IpAddr> {
		// `/assets/refresh` is an admin endpoint, and does not match.
		if path.starts_with("/asset/") {
			&self.assets
		} else {
			&self.default
		}
	}
}

/// What the rate limiting middleware needs: the limits themselves, and the
/// tokens that lift them.
#[derive(Debug, Clone)]
pub(in crate::api) struct RateLimitState {
	pub(in crate::api) limits: RequestLimits,
	pub(in crate::api) tokens: ApiTokens,
}

/// Limits every request by client address, leaving the tighter per-player
/// limits to the handlers that need them.
pub(in crate::api) async fn limit_by_address(
	State(state): State<RateLimitState>,
	client_ip: Result<ClientIp, axum_client_ip::Rejection>,
	request: Request,
	next: Next,
) -> Response {
	let path = request.uri().path();
	let token = request
		.headers()
		.get(TOKEN_HEADER)
		.and_then(|token| token.to_str().ok());

	let Ok(ClientIp(ip)) = client_ip else {
		tracing::debug!("Unable to resolve the client's address; not rate limiting");
		return next.run(request).await;
	};
	let ip = address_key(ip);

	if let Some(token) = token
		&& state.tokens.exempts(token, path, ip).await
	{
		return next.run(request).await;
	}

	match state.limits.for_path(path).check(ip).await {
		Some(retry_after) => (
			StatusCode::TOO_MANY_REQUESTS,
			[(header::RETRY_AFTER, retry_after.as_secs().to_string())],
			"Too many requests, please slow down",
		)
			.into_response(),
		None => next.run(request).await,
	}
}

#[cfg(test)]
mod tests {
	use std::{future::poll_fn, net::SocketAddr, sync::Arc, time::Duration};

	use axum::{Router, body::Body, extract::ConnectInfo, middleware, routing::get};
	use axum_client_ip::ClientIpSource;
	use sea_orm::DatabaseConnection;
	use tower::Service;

	use super::{RateLimitState, RateLimiter, Request, RequestLimits, StatusCode};
	use crate::{api::api_tokens::ApiTokens, utils::hash::sha256_hex};

	#[tokio::test]
	async fn hits_over_the_limit_are_rejected() {
		let limiter = RateLimiter::new(2, Duration::from_secs(60));

		assert!(limiter.check("a").await.is_none());
		assert!(limiter.check("a").await.is_none());

		let retry_after = limiter
			.check("a")
			.await
			.expect("the third hit is over the limit");
		assert!(retry_after > Duration::ZERO && retry_after <= Duration::from_secs(60));

		// Keys are counted independently.
		assert!(limiter.check("b").await.is_none());
	}

	/// The token is pre-resolved into the cache, so the disconnected database
	/// is never reached.
	async fn test_state() -> RateLimitState {
		let tokens = ApiTokens::new(DatabaseConnection::Disconnected);
		tokens
			.resolved
			.insert(
				sha256_hex(b"asset-reader"),
				Arc::from(vec!["/asset/".to_owned()]),
			)
			.await;
		// Cached as unresolvable, so the disconnected database stays untouched.
		tokens.rejected.insert(sha256_hex(b"revoked"), ()).await;

		RateLimitState {
			limits: RequestLimits {
				default: RateLimiter::new(1, Duration::from_secs(60)),
				assets: RateLimiter::new(3, Duration::from_secs(60)),
			},
			tokens,
		}
	}

	/// Covers the wiring as much as the limit: the middleware silently stops
	/// limiting if it cannot resolve an address, so the layer order that puts
	/// the ip source in the extensions first has to hold.
	#[tokio::test]
	async fn the_middleware_limits_by_address_path_and_token() {
		let mut app = Router::new()
			.route("/", get(async || "ok"))
			.route("/asset/{id}", get(async || "ok"))
			.layer(middleware::from_fn_with_state(
				test_state().await,
				super::limit_by_address,
			))
			.layer(ClientIpSource::ConnectInfo.into_extension())
			.into_service::<Body>();

		let mut send = async |addr: &str, path: &str, token: Option<&str>| {
			let mut request = Request::new(Body::empty());
			*request.uri_mut() = path.parse().expect("the test paths are valid");
			request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
				addr.parse().expect("the test addresses are valid"),
				1234,
			)));
			if let Some(token) = token {
				request.headers_mut().insert(
					super::TOKEN_HEADER,
					token.parse().expect("the test tokens are valid headers"),
				);
			}

			poll_fn(|cx| app.poll_ready(cx))
				.await
				.expect("the router is always ready");
			app.call(request)
				.await
				.expect("the router is infallible")
				.status()
		};

		assert_eq!(send("10.0.0.1", "/", None).await, StatusCode::OK);
		assert_eq!(
			send("10.0.0.1", "/", None).await,
			StatusCode::TOO_MANY_REQUESTS
		);
		assert_eq!(send("10.0.0.2", "/", None).await, StatusCode::OK);
		assert_eq!(
			send("::ffff:10.0.0.2", "/", None).await,
			StatusCode::TOO_MANY_REQUESTS
		);

		// A whole IPv6 /64 is one address.
		assert_eq!(send("2001:db8::1", "/", None).await, StatusCode::OK);
		assert_eq!(
			send("2001:db8::2", "/", None).await,
			StatusCode::TOO_MANY_REQUESTS
		);
		assert_eq!(send("2001:db8:0:1::1", "/", None).await, StatusCode::OK);

		// Assets are counted against their own, looser limit.
		for _ in 0..3 {
			assert_eq!(send("10.0.0.1", "/asset/7", None).await, StatusCode::OK);
		}
		assert_eq!(
			send("10.0.0.1", "/asset/7", None).await,
			StatusCode::TOO_MANY_REQUESTS
		);

		// A token lifts the limit, but only on the paths it is scoped to.
		let token = Some("asset-reader");
		for _ in 0..10 {
			assert_eq!(send("10.0.0.1", "/asset/7", token).await, StatusCode::OK);
		}
		assert_eq!(
			send("10.0.0.1", "/", token).await,
			StatusCode::TOO_MANY_REQUESTS
		);

		// A revoked token is simply ignored.
		assert_eq!(
			send("10.0.0.1", "/asset/7", Some("revoked")).await,
			StatusCode::TOO_MANY_REQUESTS
		);
	}
}
