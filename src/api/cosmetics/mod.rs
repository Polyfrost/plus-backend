mod get_player;
mod list;
mod put_player;

use std::sync::Arc;

use aide::axum::ApiRouter;
use entities::sea_orm_active_enums::CosmeticType;
use moka::future::Cache;
use s3::{Bucket, error::S3Error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::ApiState;

#[derive(Clone, Debug, Serialize, JsonSchema)]
struct CosmeticInfo {
	/// The unique ID of this cosmetic
	id: i32,
	/// The type of this cosmetic
	r#type: CosmeticType,
	/// The media url for this cosmetic
	#[serde(skip_serializing_if = "Option::is_none")]
	url: Option<String>,
	#[serde(flatten)]
	cached_info: CachedCosmeticInfo
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CachedCosmeticInfo {
	/// The hash of this cosmetic. A different hash indicates
	/// the cosmetic has changed and should be redownlowded.
	/// The hash format is unspecified
	hash: String
}

impl CosmeticInfo {
	#[tracing::instrument(
		name = "convert_db_cosmetic_info",
		level = "debug",
		skip(cache, s3_bucket)
	)]
	pub async fn from_db_model(
		value: &entities::cosmetic::Model,
		cache: Cache<i32, CachedCosmeticInfo>,
		s3_bucket: Arc<Bucket>
	) -> Result<Self, S3Error> {
		Ok(Self {
			id: value.id,
			r#type: value.r#type.clone(),
			url: match &value.path {
				Some(p) => Some(s3_bucket.as_ref().presign_get(p, 604800, None).await?),
				_ => None
			},
			cached_info: if let Some(info) = cache.get(&value.id).await {
				info
			} else {
				CachedCosmeticInfo::from_db_model(value, s3_bucket).await?
			}
		})
	}
}

impl CachedCosmeticInfo {
	// sha256("null")
	const DEFAULT_HASH: &str = "37a6259cc0c1dae299a7866489dff0bd";

	#[tracing::instrument(
		name = "fetch_cached_cosmetic_info",
		level = "debug",
		skip(s3_bucket)
	)]
	pub async fn from_db_model(
		value: &entities::cosmetic::Model,
		s3_bucket: Arc<Bucket>
	) -> Result<Self, S3Error> {
		Ok(Self {
			hash: match &value.path {
				Some(path) => {
					let (headers, _) = s3_bucket.head_object(path).await?;
					match headers.e_tag {
						None => Self::DEFAULT_HASH.to_string(),
						Some(ref etag) => {
							let etag = etag.strip_prefix("W/").unwrap_or(etag);
							let etag = etag.strip_prefix('"').unwrap_or(etag);
							let etag = etag.strip_suffix('"').unwrap_or(etag);

							etag.to_string()
						}
					}
				}
				None => Self::DEFAULT_HASH.to_string()
			}
		})
	}
}

macro_rules! gen_active_cosmetics_structs {
	($($name:ident),+) => {
		#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
		pub struct ActiveCosmetics {
			$(
				#[doc = "The ID of the active "]
				#[doc = stringify!($name)]
				#[doc = ", or null to disable"]
				$name: Option<i32>
			),+
		}

		impl ActiveCosmetics {
			pub const NAMES: [&str; { [$(stringify!($name)),+].len() }] = [$(stringify!($name)),+];
		}

		/// An object of partially-set active cosmetics.
		///
		/// Omitting a key keeps it the same, using `null` unsets it, and passing a value sets that as the active.
		#[derive(Debug, Serialize, Deserialize, JsonSchema)]
		pub struct PartialActiveCosmetics {
			$(
				#[doc = "The ID of the active "]
				#[doc = stringify!($name)]
				#[doc = ", or null to disable"]
				#[serde(
					default,
					skip_serializing_if = "Option::is_none",
					serialize_with = "::serde_with::rust::double_option::serialize",
					deserialize_with = "::serde_with::rust::double_option::deserialize",
				)]
				$name: Option<Option<i32>>
			),+
		}

		impl IntoIterator for &PartialActiveCosmetics {
			type Item = (&'static str, Option<i32>);
			type IntoIter = std::iter::Flatten<
				std::array::IntoIter<Option<(&'static str, Option<i32>)>, { ActiveCosmetics::NAMES.len() }>
			>;

			fn into_iter(self) -> Self::IntoIter {
				[$(self.$name.map(|v| (stringify!($name), v))),+].into_iter().flatten()
			}
		}
	};
}

gen_active_cosmetics_structs!(cape);

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest(
			"/cosmetics",
			ApiRouter::new()
				.merge(get_player::router())
				.merge(put_player::router())
		)
		.merge(list::router())
}
