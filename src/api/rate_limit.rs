use std::{
	fmt::Display,
	hash::Hash,
	net::{IpAddr, Ipv6Addr, SocketAddr},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
	extract::{ConnectInfo, Request, State},
	http::{StatusCode, header},
	middleware::Next,
	response::{IntoResponse, Response},
};
use axum_client_ip::{ClientIp, Rejection};
use moka::{
	Entry,
	future::Cache,
	ops::compute::{CompResult, Op},
};

use crate::api::{
	api_tokens::{ApiTokens, TOKEN_HEADER},
	lockouts::{Lockout, LockoutLog},
};

/// The longest a repeat offender is locked out for.
const MAX_PENALTY: Duration = Duration::from_secs(60 * 60);

/// Routes that authenticate their caller themselves, such as signed webhooks.
/// Only the requests they reject count against the caller's limit.
const SELF_VERIFYING_PATHS: [&str; 1] = ["/v1/paynow/webhook"];

pub(in crate::api) trait LimitKey:
	Hash + Eq + Clone + Display + Send + Sync + 'static
{
}

impl<K: Hash + Eq + Clone + Display + Send + Sync + 'static> LimitKey for K {}

#[derive(Debug, Clone, Copy)]
struct Penalty {
	/// Unix seconds the lockout ends at.
	until: u64,
	/// How long it lasted, doubled on the next offence.
	timeout: u64,
}

impl Penalty {
	fn starting(now: u64, timeout: u64) -> Self {
		let timeout = timeout.min(MAX_PENALTY.as_secs());
		Self {
			until: now + timeout,
			timeout,
		}
	}
}

fn unix_now() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs()
}

// TODO: Redis?
#[derive(Debug, Clone)]
pub(in crate::api) struct RateLimiter<K: LimitKey> {
	name: &'static str,
	hits: Cache<(K, u64), u32>,
	penalties: Cache<K, Penalty>,
	window: Duration,
	limit: u32,
	lockouts: LockoutLog,
}

impl<K: LimitKey> RateLimiter<K> {
	/// `name` identifies the limiter in the lockout log.
	pub(in crate::api) fn new(
		name: &'static str,
		limit: u32,
		window: Duration,
		lockouts: LockoutLog,
	) -> Self {
		assert!(
			window.as_secs() > 0,
			"the rate limit window must be whole seconds"
		);

		RateLimiter {
			name,
			// Two windows, so the window being filled never evicts itself.
			hits: Cache::builder().time_to_live(window * 2).build(),
			// Long enough to still remember a lockout once it is forgiven.
			penalties: Cache::builder().time_to_live(MAX_PENALTY * 2).build(),
			window,
			limit,
			lockouts,
		}
	}

	/// Records a hit, returning how long to wait once the key is over its
	/// limit. Each lockout lasts at least the rest of the window, and twice the
	/// previous one if the key offends again before behaving for that long.
	pub(in crate::api) async fn check(&self, key: K) -> Option<Duration> {
		self.check_at(key, unix_now()).await
	}

	/// How long the key is still locked out for, without recording a hit.
	pub(in crate::api) async fn lockout(&self, key: &K) -> Option<Duration> {
		self.lockout_at(key, unix_now()).await
	}

	async fn lockout_at(&self, key: &K, now: u64) -> Option<Duration> {
		let penalty = self.penalties.get(key).await?;
		(now < penalty.until).then(|| Duration::from_secs(penalty.until - now))
	}

	async fn check_at(&self, key: K, now: u64) -> Option<Duration> {
		if let Some(wait) = self.lockout_at(&key, now).await {
			return Some(wait);
		}

		let window = self.window.as_secs();
		let hits = self
			.hits
			.entry((key.clone(), now / window))
			.and_upsert_with(|entry| async move {
				entry.map_or(1, |entry| entry.into_value().saturating_add(1))
			})
			.await
			.into_value();
		if hits <= self.limit {
			return None;
		}

		let rest_of_window = window - now % window;
		// Atomic, so a burst of concurrent hits doubles the lockout only once.
		let result = self
			.penalties
			.entry(key.clone())
			.and_compute_with(|entry| async move {
				match entry.map(Entry::into_value) {
					Some(penalty) if now < penalty.until => Op::Nop,
					Some(penalty) if now < penalty.until + penalty.timeout => Op::Put(
						Penalty::starting(now, rest_of_window.max(penalty.timeout * 2)),
					),
					_ => Op::Put(Penalty::starting(now, rest_of_window)),
				}
			})
			.await;

		let penalty = match result {
			CompResult::Inserted(entry) | CompResult::ReplacedWith(entry) => {
				let penalty = entry.into_value();
				self.lockouts.record(Lockout {
					limiter: self.name,
					key: key.to_string(),
					timeout: Duration::from_secs(penalty.timeout),
					limit: self.limit,
					window: self.window,
				});
				penalty
			}
			CompResult::Unchanged(entry) | CompResult::Removed(entry) => {
				entry.into_value()
			}
			CompResult::StillNone(_) => Penalty::starting(now, rest_of_window),
		};

		Some(Duration::from_secs(penalty.until - now))
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

fn too_many_requests(retry_after: Duration) -> Response {
	(
		StatusCode::TOO_MANY_REQUESTS,
		[(header::RETRY_AFTER, retry_after.as_secs().to_string())],
		"Too many requests, please slow down",
	)
		.into_response()
}

/// Limits every request by client address, leaving the tighter per-player
/// limits to the handlers that need them.
pub(in crate::api) async fn limit_by_address(
	State(state): State<RateLimitState>,
	client_ip: Result<ClientIp, Rejection>,
	request: Request,
	next: Next,
) -> Response {
	let path = request.uri().path();
	let token = request
		.headers()
		.get(TOKEN_HEADER)
		.and_then(|token| token.to_str().ok());

	// A header source missing its header falls back to the peer, so requests
	// that skip the proxy share its limit rather than going unlimited.
	let peer = request
		.extensions()
		.get::<ConnectInfo<SocketAddr>>()
		.map(|ConnectInfo(addr)| addr.ip());
	let Some(ip) = client_ip.ok().map(|ClientIp(ip)| ip).or(peer) else {
		tracing::warn!("Unable to resolve the client's address; not rate limiting");
		return next.run(request).await;
	};
	let ip = address_key(ip);

	if let Some(token) = token
		&& state.tokens.exempts(token, path, ip).await
	{
		return next.run(request).await;
	}

	let limiter = state.limits.for_path(path);

	if SELF_VERIFYING_PATHS.contains(&path) {
		if let Some(retry_after) = limiter.lockout(&ip).await {
			return too_many_requests(retry_after);
		}
		let response = next.run(request).await;
		if response.status() == StatusCode::UNAUTHORIZED {
			limiter.check(ip).await;
		}
		return response;
	}

	match limiter.check(ip).await {
		Some(retry_after) => too_many_requests(retry_after),
		None => next.run(request).await,
	}
}

#[cfg(test)]
mod tests {
	use std::{future::poll_fn, net::SocketAddr, sync::Arc, time::Duration};

	use axum::{
		Router,
		body::Body,
		extract::ConnectInfo,
		http::{HeaderMap, HeaderValue, Method},
		middleware,
		routing::{get, post},
	};
	use axum_client_ip::ClientIpSource;
	use sea_orm::DatabaseConnection;
	use tower::Service;

	use super::{RateLimitState, RateLimiter, Request, RequestLimits, StatusCode};
	use crate::{
		api::{api_tokens::ApiTokens, lockouts::LockoutLog},
		utils::hash::sha256_hex,
	};

	#[tokio::test]
	async fn hits_over_the_limit_are_rejected() {
		let limiter =
			RateLimiter::new("test", 2, Duration::from_secs(60), LockoutLog::default());

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

	#[tokio::test]
	async fn repeat_offences_double_the_lockout() {
		let limiter =
			RateLimiter::new("test", 1, Duration::from_secs(60), LockoutLog::default());
		let secs = |secs| Some(Duration::from_secs(secs));

		assert!(limiter.check_at("a", 0).await.is_none());
		assert_eq!(limiter.check_at("a", 0).await, secs(60));
		// Hits while locked out neither count nor extend it.
		assert_eq!(limiter.check_at("a", 30).await, secs(30));

		assert!(limiter.check_at("a", 60).await.is_none());
		assert_eq!(limiter.check_at("a", 60).await, secs(120));

		assert!(limiter.check_at("a", 180).await.is_none());
		assert_eq!(limiter.check_at("a", 180).await, secs(240));

		// Behaving for as long as the last lockout forgives it.
		assert!(limiter.check_at("a", 900).await.is_none());
		assert_eq!(limiter.check_at("a", 900).await, secs(60));

		// Other keys are untouched.
		assert!(limiter.check_at("b", 900).await.is_none());
	}

	#[tokio::test]
	async fn each_lockout_is_logged_once() {
		let (log, mut lockouts) = LockoutLog::capture();
		let limiter = RateLimiter::new("test", 1, Duration::from_secs(60), log);

		assert!(limiter.check_at("a", 0).await.is_none());
		for _ in 0..5 {
			assert!(limiter.check_at("a", 0).await.is_some());
		}
		assert!(limiter.check_at("a", 60).await.is_none());
		assert!(limiter.check_at("a", 60).await.is_some());

		let first = lockouts.try_recv().expect("the first lockout is logged");
		assert_eq!((first.limiter, first.key.as_str()), ("test", "a"));
		assert_eq!(first.timeout, Duration::from_secs(60));
		let second = lockouts.try_recv().expect("the doubled lockout is logged");
		assert_eq!(second.timeout, Duration::from_secs(120));
		assert!(lockouts.try_recv().is_err());
	}

	/// The token is pre-resolved into the cache, so the disconnected database
	/// is never reached.
	async fn test_state() -> RateLimitState {
		let tokens =
			ApiTokens::new(DatabaseConnection::Disconnected, &LockoutLog::default());
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
				default: RateLimiter::new(
					"test",
					1,
					Duration::from_secs(60),
					LockoutLog::default(),
				),
				assets: RateLimiter::new(
					"test",
					3,
					Duration::from_secs(60),
					LockoutLog::default(),
				),
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

	#[tokio::test]
	async fn requests_missing_the_proxy_header_are_limited_by_peer() {
		let mut app = Router::new()
			.route("/", get(async || "ok"))
			.layer(middleware::from_fn_with_state(
				test_state().await,
				super::limit_by_address,
			))
			.layer(ClientIpSource::RightmostXForwardedFor.into_extension())
			.into_service::<Body>();

		let mut send = async || {
			let mut request = Request::new(Body::empty());
			request
				.extensions_mut()
				.insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 9], 1234))));

			poll_fn(|cx| app.poll_ready(cx))
				.await
				.expect("the router is always ready");
			app.call(request)
				.await
				.expect("the router is infallible")
				.status()
		};

		assert_eq!(send().await, StatusCode::OK);
		assert_eq!(send().await, StatusCode::TOO_MANY_REQUESTS);
	}

	/// Signed webhooks are never limited; only the requests the route rejects
	/// as unauthenticated count, and a lockout then stops even signed ones.
	#[tokio::test]
	async fn self_verifying_routes_only_count_rejections() {
		let mut app = Router::new()
			.route(
				"/v1/paynow/webhook",
				post(async |headers: HeaderMap| {
					if headers.contains_key("x-signed") {
						StatusCode::OK
					} else {
						StatusCode::UNAUTHORIZED
					}
				}),
			)
			.layer(middleware::from_fn_with_state(
				test_state().await,
				super::limit_by_address,
			))
			.layer(ClientIpSource::ConnectInfo.into_extension())
			.into_service::<Body>();

		let mut send = async |signed: bool| {
			let mut request = Request::new(Body::empty());
			*request.method_mut() = Method::POST;
			*request.uri_mut() = "/v1/paynow/webhook"
				.parse()
				.expect("the test path is valid");
			request
				.extensions_mut()
				.insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 7], 1234))));
			if signed {
				request
					.headers_mut()
					.insert("x-signed", HeaderValue::from_static("1"));
			}

			poll_fn(|cx| app.poll_ready(cx))
				.await
				.expect("the router is always ready");
			app.call(request)
				.await
				.expect("the router is infallible")
				.status()
		};

		for _ in 0..5 {
			assert_eq!(send(true).await, StatusCode::OK);
		}
		// The default limit in the test state is one hit.
		assert_eq!(send(false).await, StatusCode::UNAUTHORIZED);
		assert_eq!(send(false).await, StatusCode::UNAUTHORIZED);
		assert_eq!(send(true).await, StatusCode::TOO_MANY_REQUESTS);
	}
}
