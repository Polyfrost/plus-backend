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

#[derive(Debug, Clone)]
pub(in crate::api) struct ApiTokens {
	database: DatabaseConnection,
	pub(in crate::api) resolved: Cache<String, Arc<[String]>>,
	/// Hashes that resolved to nothing are kept apart so junk tokens cannot evict valid ones
	pub(in crate::api) rejected: Cache<String, ()>,
	lookups: RateLimiter<IpAddr>,
}

pub(in crate::api) struct GeneratedToken {
	pub(in crate::api) token: String,
	pub(in crate::api) hash: String,
}

impl ApiTokens {
	pub(in crate::api) fn new(database: DatabaseConnection) -> Self {
		ApiTokens {
			database,
			resolved: Cache::builder()
				.time_to_live(CACHE_TTL)
				.max_capacity(CACHE_CAPACITY)
				.build(),
			rejected: Cache::builder()
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

	async fn prefixes(&self, token: &str, ip: IpAddr) -> Option<Arc<[String]>> {
		let hash = sha256_hex(token.as_bytes());

		if let Some(prefixes) = self.resolved.get(&hash).await {
			return Some(prefixes);
		}
		if self.rejected.contains_key(&hash) {
			return None;
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

		match found {
			Ok(Some(token)) => {
				let prefixes: Arc<[String]> = Arc::from(token.exempt_prefixes);
				self.resolved.insert(hash, prefixes.clone()).await;
				Some(prefixes)
			}
			Ok(None) => {
				self.rejected.insert(hash, ()).await;
				None
			}
			Err(error) => {
				tracing::warn!(%error, "Unable to resolve an api token");
				None
			}
		}
	}
}

pub(in crate::api) fn generate_token() -> GeneratedToken {
	let mut bytes = [0u8; 32];
	rand::rng().fill_bytes(&mut bytes);

	let token = URL_SAFE_NO_PAD.encode(bytes);
	let hash = sha256_hex(token.as_bytes());

	GeneratedToken { token, hash }
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

	#[tokio::test]
	async fn a_cached_token_never_spends_the_lookup_budget() {
		let tokens = ApiTokens::new(DatabaseConnection::Disconnected);
		tokens
			.resolved
			.insert(sha256_hex(b"busy"), Arc::from(vec!["/asset/".to_owned()]))
			.await;

		for _ in 0..(super::LOOKUPS_PER_WINDOW * 10) {
			assert!(tokens.exempts("busy", "/asset/7", IP).await);
		}
	}

	#[test]
	fn generated_tokens_are_unique_and_hashed() {
		let generated = generate_token();
		let other = generate_token();

		assert_ne!(generated.token, other.token);
		assert_ne!(generated.hash, other.hash);
		assert_eq!(generated.hash, sha256_hex(generated.token.as_bytes()));
		assert!(!generated.hash.contains(&generated.token));
	}
}
