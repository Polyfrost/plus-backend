mod get_player;
mod grant;
mod grant_emote;
mod list;
mod list_capes;
mod list_emotes;
mod put_player;
mod upload_cape;
mod upload_emote;

use std::{collections::HashMap, sync::Arc};

use aide::axum::ApiRouter;
use entities::{
	asset,
	sea_orm_active_enums::CosmeticType,
};
use moka::future::Cache;
use s3::{Bucket, error::S3Error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::ApiState;

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(super) struct CosmeticInfo {
	/// The unique ID of this cosmetic
	id: i32,
	/// The type of this cosmetic
	r#type: CosmeticType,
	/// The display name for this cosmetic
	name: String,
	/// The media url for this cosmetic
	#[serde(skip_serializing_if = "Option::is_none")]
	url: Option<String>,
	#[serde(flatten)]
	cached_info: CachedAssetInfo,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(super) struct EmoteInfo {
	/// The unique ID of this emote
	id: i32,
	/// The display name for this emote
	name: String,
	/// The emote bundle URL
	#[serde(skip_serializing_if = "Option::is_none")]
	url: Option<String>,
	#[serde(flatten)]
	cached_info: CachedAssetInfo,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CachedAssetInfo {
	/// The hash of this cosmetic. A different hash indicates
	/// the cosmetic has changed and should be redownlowded.
	/// The hash format is unspecified
	hash: String,
}

impl CosmeticInfo {
	#[tracing::instrument(
		name = "convert_db_cosmetic_info",
		level = "debug",
		skip(cache, s3_bucket)
	)]
	pub async fn from_db_model(
		value: &entities::cosmetic::Model,
		asset: Option<&asset::Model>,
		cache: Cache<i32, CachedAssetInfo>,
		s3_bucket: Arc<Bucket>,
	) -> Result<Self, S3Error> {
		let cached_info =
			CachedAssetInfo::from_optional_asset(asset, cache, s3_bucket.clone()).await?;
		Ok(Self {
			id: value.id,
			r#type: value.r#type.clone(),
			name: value.name.clone(),
			url: CachedAssetInfo::asset_url(asset, s3_bucket).await?,
			cached_info,
		})
	}
}

impl EmoteInfo {
	#[tracing::instrument(
		name = "convert_db_emote_info",
		level = "debug",
		skip(cache, s3_bucket)
	)]
	pub async fn from_db_model(
		value: &entities::emote::Model,
		asset: Option<&asset::Model>,
		cache: Cache<i32, CachedAssetInfo>,
		s3_bucket: Arc<Bucket>,
	) -> Result<Self, S3Error> {
		let cached_info =
			CachedAssetInfo::from_optional_asset(asset, cache, s3_bucket.clone()).await?;
		Ok(Self {
			id: value.id,
			name: value.name.clone(),
			url: CachedAssetInfo::asset_url(asset, s3_bucket).await?,
			cached_info,
		})
	}
}

impl CachedAssetInfo {
	// sha256("null")
	const DEFAULT_HASH: &str = "37a6259cc0c1dae299a7866489dff0bd";

	#[tracing::instrument(
		name = "fetch_cached_asset_info",
		level = "debug",
		skip(s3_bucket)
	)]
	pub async fn from_db_model(
		value: &asset::Model,
		s3_bucket: Arc<Bucket>,
	) -> Result<Self, S3Error> {
		Ok(Self {
			hash: match (&value.hash, &value.storage_path) {
				(Some(hash), _) => hash.clone(),
				(None, Some(path)) => {
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
				(None, None) => Self::DEFAULT_HASH.to_string(),
			},
		})
	}

	async fn from_optional_asset(
		asset: Option<&asset::Model>,
		cache: Cache<i32, CachedAssetInfo>,
		s3_bucket: Arc<Bucket>,
	) -> Result<Self, S3Error> {
		let Some(asset) = asset else {
			return Ok(Self {
				hash: Self::DEFAULT_HASH.to_string(),
			});
		};

		if let Some(info) = cache.get(&asset.id).await {
			return Ok(info);
		}

		Self::from_db_model(asset, s3_bucket).await
	}

	async fn asset_url(
		asset: Option<&asset::Model>,
		s3_bucket: Arc<Bucket>,
	) -> Result<Option<String>, S3Error> {
		let Some(asset) = asset else {
			return Ok(None);
		};

		if let Some(url) = &asset.url {
			return Ok(Some(url.clone()));
		}

		match &asset.storage_path {
			Some(path) => Ok(Some(
				s3_bucket.as_ref().presign_get(path, 604800, None).await?,
			)),
			None => Ok(None),
		}
	}
}

/// Current equipment keyed by cosmetic type.
pub(super) type EquippedCosmetics = HashMap<CosmeticType, i32>;

/// Partial equipment updates. Missing slots are left unchanged, while a `null`
/// value unequips the slot.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub(super) struct PartialEquippedCosmetics {
	pub equipped: HashMap<CosmeticType, Option<i32>>,
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest(
			"/cosmetics",
			ApiRouter::new()
				.merge(get_player::router())
				.merge(put_player::router())
				.merge(upload_cape::router())
				.merge(upload_emote::router())
				.merge(grant::router())
				.merge(grant_emote::router())
				.merge(list_capes::router()),
		)
		.merge(list_emotes::router())
		.merge(list::router())
}
