mod cover;
mod get_player;
mod grant;
mod list;
mod list_capes;
mod manage;
mod put_player;
mod search;
mod view;

use std::{
	collections::{BTreeMap, HashMap},
	sync::Arc,
};

use aide::axum::ApiRouter;
use entities::{
	asset, cosmetic, cosmetic_group,
	sea_orm_active_enums::{BodySlot, CosmeticType},
};
use moka::future::Cache;
use s3::{Bucket, error::S3Error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::ApiState;

pub(super) fn is_zip(data: &[u8]) -> bool {
	data.len() >= 4 && &data[0..4] == b"PK\x03\x04"
}

fn is_macos_junk(name: &str) -> bool {
	name.split('/')
		.next_back()
		.is_some_and(|base| base == ".DS_Store")
		|| name.starts_with("__MACOSX/")
		|| name.contains("/__MACOSX/")
}

pub(super) fn strip_macos_junk(data: &[u8]) -> Result<Vec<u8>, zip::result::ZipError> {
	use std::io::{Cursor, Read, Write};

	let mut archive = zip::ZipArchive::new(Cursor::new(data))?;
	let mut out = Cursor::new(Vec::new());
	{
		let mut writer = zip::ZipWriter::new(&mut out);
		for i in 0..archive.len() {
			let mut entry = archive.by_index(i)?;
			let name = entry.name().to_string();
			if entry.is_dir() || is_macos_junk(&name) {
				continue;
			}
			let options = zip::write::SimpleFileOptions::default()
				.compression_method(zip::CompressionMethod::Deflated);
			writer.start_file(name, options)?;
			let mut buf = Vec::with_capacity(entry.size() as usize);
			entry.read_to_end(&mut buf)?;
			writer.write_all(&buf)?;
		}
		writer.finish()?;
	}
	Ok(out.into_inner())
}

/// A buyable cosmetic. What the player owns once and chooses variants within.
///
/// When a cosmetic has multiple variants (e.g. every pride cape, or every cat
/// ear color), they are grouped here under one entry; `variants` lists the
/// individual choices. A cosmetic with no group is emitted as a single-variant
/// entry. Owning/granting this cosmetic grants every variant beneath it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(super) struct CosmeticInfo {
	/// The unique ID of this cosmetic. For a grouped cosmetic this is the group
	/// id; for an ungrouped cosmetic it is the cosmetic's own id.
	id: i32,
	/// The type of this cosmetic
	r#type: CosmeticType,
	/// The display name for this cosmetic
	name: String,
	/// The body slots any variant of this cosmetic may be equipped in. A glove,
	/// for example, may allow one or both hands; the client must equip into one
	/// of these.
	allowed_slots: Vec<BodySlot>,
	/// The selectable variants of this cosmetic. Always contains at least one
	/// entry. Equip/ownership/websocket operations use the variant's `id`.
	variants: Vec<VariantInfo>,
}

/// A single selectable variant of a [`CosmeticInfo`].
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(super) struct VariantInfo {
	/// The equippable cosmetic ID for this variant. This is the id used in
	/// equip, ownership, and websocket operations.
	id: i32,
	/// The display name for this variant (e.g. "Blue", "Left Silver"). For an
	/// ungrouped cosmetic this is the cosmetic's own name.
	name: String,
	/// The skin model this variant targets ("slim"/"wide"), when the client
	/// must pick a model to match the player's skin. Absent when the variant is
	/// model-independent.
	#[serde(skip_serializing_if = "Option::is_none")]
	model: Option<String>,
	/// The body slots THIS variant may be equipped in. Usually equals the
	/// group's `allowed_slots`, but a variant can be narrower — e.g. a "Left"
	/// gauntlet only allows `left_hand` while the group allows both hands. The
	/// client must equip into one of these.
	allowed_slots: Vec<BodySlot>,
	/// The media url for this variant
	#[serde(skip_serializing_if = "Option::is_none")]
	url: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	cover_url: Option<String>,
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

impl VariantInfo {
	#[tracing::instrument(
		name = "convert_db_variant_info",
		level = "debug",
		skip(cache, s3_bucket)
	)]
	async fn from_db_model(
		value: &cosmetic::Model,
		asset: Option<&asset::Model>,
		cover_asset: Option<&asset::Model>,
		allowed_slots: Vec<BodySlot>,
		cache: Cache<i32, CachedAssetInfo>,
		s3_bucket: Arc<Bucket>,
	) -> Result<Self, S3Error> {
		let cached_info =
			CachedAssetInfo::from_optional_asset(asset, cache, s3_bucket.clone()).await?;
		Ok(Self {
			id: value.id,
			allowed_slots,
			name: value
				.variant_name
				.clone()
				.or_else(|| value.name.clone())
				.unwrap_or_else(|| format!("Cosmetic {}", value.id)),
			model: value.model_variant.clone(),
			url: CachedAssetInfo::asset_url(asset, s3_bucket.clone()).await?,
			cover_url: CachedAssetInfo::asset_url(cover_asset, s3_bucket).await?,
			cached_info,
		})
	}
}

pub(super) async fn load_groups<C: sea_orm::ConnectionTrait>(
	db: &C,
) -> Result<HashMap<i32, (cosmetic_group::Model, Vec<BodySlot>)>, sea_orm::DbErr> {
	use entities::prelude::{CosmeticGroup, CosmeticGroupAllowedSlot};
	use sea_orm::EntityTrait;

	Ok(CosmeticGroup::find()
		.find_with_related(CosmeticGroupAllowedSlot)
		.all(db)
		.await?
		.into_iter()
		.map(|(group, slots)| {
			(group.id, (group, slots.into_iter().map(|s| s.slot).collect()))
		})
		.collect())
}

pub(super) type CosmeticRow =
	(cosmetic::Model, Option<asset::Model>, Option<asset::Model>, Vec<BodySlot>);

pub(super) async fn group_cosmetics(
	cosmetics: Vec<CosmeticRow>,
	groups: HashMap<i32, (cosmetic_group::Model, Vec<BodySlot>)>,
	cache: Cache<i32, CachedAssetInfo>,
	s3_bucket: Arc<Bucket>,
) -> Result<Vec<CosmeticInfo>, S3Error> {
	let mut buckets: BTreeMap<i32, (CosmeticInfo, Vec<(i32, VariantInfo)>)> =
		BTreeMap::new();

	for (cosmetic, asset, cover_asset, allowed_slots) in cosmetics {
		let variant = VariantInfo::from_db_model(
			&cosmetic,
			asset.as_ref(),
			cover_asset.as_ref(),
			allowed_slots.clone(),
			cache.clone(),
			s3_bucket.clone(),
		)
		.await?;

		let group = cosmetic.group_id.and_then(|id| groups.get(&id).map(|g| (id, g)));
		let (bucket_id, entry) = match group {
			Some((group_id, (group, group_slots))) => (
				group_id,
				CosmeticInfo {
					id: group_id,
					r#type: group.r#type.clone(),
					name: group.name.clone(),
					allowed_slots: group_slots.clone(),
					variants: Vec::new(),
				},
			),
			None => (
				cosmetic.id,
				CosmeticInfo {
					id: cosmetic.id,
					r#type: cosmetic.r#type.clone(),
					name: cosmetic
						.name
						.clone()
						.unwrap_or_else(|| format!("Cosmetic {}", cosmetic.id)),
					allowed_slots,
					variants: Vec::new(),
				},
			),
		};

		buckets
			.entry(bucket_id)
			.or_insert((entry, Vec::new()))
			.1
			.push((cosmetic.variant_order, variant));
	}

	Ok(buckets
		.into_values()
		.map(|(mut entry, mut variants)| {
			variants.sort_by_key(|(order, variant)| (*order, variant.id));
			entry.variants = variants.into_iter().map(|(_, variant)| variant).collect();
			entry
		})
		.collect())
}

impl EmoteInfo {
	#[tracing::instrument(
		name = "convert_db_emote_info",
		level = "debug",
		skip(cache, s3_bucket)
	)]
	pub async fn from_db_model(
		value: &cosmetic::Model,
		asset: Option<&asset::Model>,
		cache: Cache<i32, CachedAssetInfo>,
		s3_bucket: Arc<Bucket>,
	) -> Result<Self, S3Error> {
		let cached_info =
			CachedAssetInfo::from_optional_asset(asset, cache, s3_bucket.clone()).await?;
		Ok(Self {
			id: value.id,
			name: value
				.name
				.clone()
				.unwrap_or_else(|| format!("Emote {}", value.id)),
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

/// Current equipment keyed by body slot.
pub(super) type EquippedCosmetics = HashMap<BodySlot, i32>;

/// Partial equipment updates. Missing slots are left unchanged, while a `null`
/// value unequips the slot.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub(super) struct PartialEquippedCosmetics {
	pub equipped: HashMap<BodySlot, Option<i32>>,
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest(
			"/cosmetics",
			ApiRouter::new()
				.merge(get_player::router())
				.merge(put_player::router())
				.merge(manage::router())
				.merge(grant::router())
				.merge(list_capes::router()),
		)
		.merge(list::router())
		.merge(search::router())
		.merge(view::router())
}
