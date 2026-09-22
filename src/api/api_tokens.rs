use std::{net::IpAddr, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use entities::{api_token, prelude::*};
use moka::future::Cache;
use rand::RngCore as _;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::{api::rate_limit::RateLimiter, utils::hash::sha256_hex};

pub(in crate::api) const TOKEN_HEADER: &str = "X-Api-Token";
pub(in crate::api) const OPENAPI_SECURITY_NAME: &str = "API Token";

const CACHE_TTL: Duration = Duration::from_secs(60);
const CACHE_CAPACITY: u64 = 1024;
const LOOKUPS_PER_WINDOW: u32 = 60;
const LOOKUP_WINDOW: Duration = Duration::from_secs(60);

/// Prefixes of a token that is not (or no longer) valid.
type Prefixes = Option<Arc<[String]>>;

#[derive(Debug, Clone)]
pub(in crate::api) struct ApiTokens {
	database: DatabaseConnection,
	pub(in crate::api) resolved: Cache<String, Prefixes>,
	lookups: RateLimiter<IpAddr>,
}

impl ApiTokens {
	pub(in crate::api) fn new(database: DatabaseConnection) -> Self {
		ApiTokens {
			database,
			resolved: Cache::builder()
				.time_to_live(CACHE_TTL)
				.max_capacity(CACHE_CAPACITY)
				.build(),
			lookups: RateLimiter::new(LOOKUPS_PER_WINDOW, LOOKUP_WINDOW),
		}
	}

	/// Whether `token` lifts the rate limit on `path`.
	pub(in crate::api) async fn exempts(
		&self,
		token: &str,
		path: &str,
		ip: IpAddr,
	) -> bool {
		let Some(prefixes) = self.prefixes(token, ip).await else {
			return false;
		};

		prefixes
			.iter()
			.any(|prefix| path.starts_with(prefix.as_str()))
	}

	async fn prefixes(&self, token: &str, ip: IpAddr) -> Prefixes {
		let hash = sha256_hex(token.as_bytes());

		if let Some(prefixes) = self.resolved.get(&hash).await {
			return prefixes;
		}

		if self.lookups.check(ip).await.is_some() {
			tracing::debug!(%ip, "Too many api token lookups - not resolving");
			return None;
		}

		let found = ApiToken::find()
			.filter(api_token::Column::TokenHash.eq(&hash))
			.filter(api_token::Column::RevokedAt.is_null())
			.one(&self.database)
			.await;

		let prefixes = match found {
			Ok(token) => token.map(|token| Arc::from(token.exempt_prefixes)),
			Err(error) => {
				tracing::warn!(%error, "Unable to resolve an api token");
				return None;
			}
		};

		self.resolved.insert(hash, prefixes.clone()).await;
		prefixes
	}

	/// Drops a token from the cache so a revocation takes effect immediately on
	/// this replica instead of after the cache ttl.
	pub(in crate::api) async fn forget(&self, token_hash: &str) {
		self.resolved.invalidate(token_hash).await;
	}
}

/// Generates a token, returning the secret to show the creator once alongside
/// the hash to store.
pub(in crate::api) fn generate_token() -> (String, String) {
	let mut bytes = [0u8; 32];
	rand::rng().fill_bytes(&mut bytes);

	let token = URL_SAFE_NO_PAD.encode(bytes);
	let hash = sha256_hex(token.as_bytes());

	(token, hash)
}

#[cfg(test)]
mod tests {
	use std::{
		net::{IpAddr, Ipv4Addr},
		sync::Arc,
	};

	use sea_orm::DatabaseConnection;

	use super::{ApiTokens, generate_token};
	use crate::utils::hash::sha256_hex;

	const IP: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

	/// Disconnected, so a test that expected to answer from the cache and
	/// instead reached the database fails loudly.
	fn tokens() -> ApiTokens {
		ApiTokens::new(DatabaseConnection::Disconnected)
	}

	async fn seed(tokens: &ApiTokens, token: &str, prefixes: &[&str]) {
		let prefixes: Vec<String> = prefixes.iter().map(|p| (*p).to_owned()).collect();
		tokens
			.resolved
			.insert(sha256_hex(token.as_bytes()), Some(Arc::from(prefixes)))
			.await;
	}

	#[tokio::test]
	async fn a_token_only_exempts_its_own_prefixes() {
		let tokens = tokens();
		seed(&tokens, "scoped", &["/asset/", "/v1/store/"]).await;

		assert!(tokens.exempts("scoped", "/asset/7", IP).await);
		assert!(tokens.exempts("scoped", "/v1/store/catalog", IP).await);
		assert!(!tokens.exempts("scoped", "/groups/1/messages", IP).await);
	}

	#[tokio::test]
	async fn a_root_prefix_exempts_everything() {
		let tokens = tokens();
		seed(&tokens, "all", &["/"]).await;

		assert!(tokens.exempts("all", "/asset/7", IP).await);
		assert!(tokens.exempts("all", "/groups/1/messages", IP).await);
	}

	/// An unknown token reaches the database, so only the cached half of
	/// "resolves to nothing" is covered here.
	#[tokio::test]
	async fn a_token_cached_as_revoked_exempts_nothing() {
		let tokens = tokens();
		tokens.resolved.insert(sha256_hex(b"revoked"), None).await;

		assert!(!tokens.exempts("revoked", "/asset/7", IP).await);
	}

	/// The lookup budget exists to bound database work, so a token the cache
	/// can answer for has to keep working however busy its holder is.
	#[tokio::test]
	async fn a_cached_token_never_spends_the_lookup_budget() {
		let tokens = tokens();
		seed(&tokens, "busy", &["/asset/"]).await;

		for _ in 0..(super::LOOKUPS_PER_WINDOW * 10) {
			assert!(tokens.exempts("busy", "/asset/7", IP).await);
		}
	}

	#[test]
	fn generated_tokens_are_unique_and_hashed() {
		let (token, hash) = generate_token();
		let (other, other_hash) = generate_token();

		assert_ne!(token, other);
		assert_ne!(hash, other_hash);
		assert_eq!(hash, sha256_hex(token.as_bytes()));
		assert!(!hash.contains(&token));
	}
}
