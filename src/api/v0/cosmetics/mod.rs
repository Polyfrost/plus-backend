pub(in crate::api) mod color_slots;
mod cover;
mod definitions;
mod effect;
mod engine;
mod get_player;
mod limits;
pub(in crate::api) mod grant;
mod list;
mod manage;
mod nameplate;
mod put_player;
mod search;
mod view;

use std::{
	collections::{BTreeMap, HashMap},
	sync::Arc,
	time::Duration,
};

use aide::axum::ApiRouter;
use entities::{
	asset, cosmetic, cosmetic_group,
	prelude::{Asset, CosmeticGroup, CosmeticGroupAllowedSlot},
	product_settings,
	sea_orm_active_enums::{BodySlot, CosmeticType},
};
use moka::future::Cache;
use s3::{Bucket, error::S3Error};
use sea_orm::{
	ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, QueryFilter,
	sea_query::Query,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
	api::{
		ApiState, state::RealtimeState, v0::websocket::structs::ClientBoundPacket,
	},
	utils::debounce::Debouncer,
};

const CATALOG_UPDATE_DELAY: Duration = Duration::from_secs(3);

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest(
			"/cosmetics",
			ApiRouter::new()
				.merge(definitions::router())
				.merge(get_player::router())
				.merge(put_player::router())
				.merge(manage::router())
				.merge(grant::router())
				.merge(search::router())
				.merge(view::router()),
		)
		.merge(list::router())
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
	#[schemars(
		description = "Whether `id` is a group id (true) or the id of an ungrouped cosmetic (false). Group ids and cosmetic ids come from separate sequences and may be equal, so clients key entries by `id` and `grouped` together."
	)]
	grouped: bool,
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
	/// group's `allowed_slots`, but a variant can be narrower - e.g. a "Left"
	/// gauntlet only allows `left_hand` while the group allows both hands. The
	/// client must equip into one of these.
	allowed_slots: Vec<BodySlot>,
	/// The media url for this variant
	#[serde(skip_serializing_if = "Option::is_none")]
	url: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	cover_url: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	variant_key: Option<String>,
	#[serde(flatten)]
	cached_info: CachedAssetInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(in crate::api) struct EmoteInfo {
	/// The unique ID of this emote
	pub(in crate::api) id: i32,
	/// The display name for this emote
	pub(in crate::api) name: String,
	/// The emote bundle URL
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(in crate::api) url: Option<String>,
	#[serde(flatten)]
	pub(in crate::api) cached_info: CachedAssetInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CachedAssetInfo {
	pub(in crate::api) hash: String,
}

impl VariantInfo {
	#[tracing::instrument(
		name = "convert_db_variant_info",
		level = "debug",
		skip(cache, s3_bucket, public_url)
	)]
	async fn from_db_model(
		value: &cosmetic::Model,
		asset: Option<&asset::Model>,
		cover_asset: Option<&asset::Model>,
		allowed_slots: Vec<BodySlot>,
		cache: Cache<i32, CachedAssetInfo>,
		s3_bucket: Arc<Bucket>,
		public_url: &str,
	) -> Result<Self, S3Error> {
		let cached_info =
			CachedAssetInfo::from_optional_asset(asset, cache, s3_bucket).await?;
		Ok(Self {
			id: value.id,
			allowed_slots,
			name: value
				.variant_name
				.clone()
				.or_else(|| value.name.clone())
				.unwrap_or_else(|| format!("Cosmetic {}", value.id)),
			model: value.model_variant.clone(),
			url: CachedAssetInfo::asset_url(asset, public_url),
			cover_url: CachedAssetInfo::asset_url(cover_asset, public_url),
			variant_key: value.variant_key.clone(),
			cached_info,
		})
	}
}

pub(in crate::api) fn spawn_catalog_notifier(realtime: RealtimeState) -> Debouncer {
	Debouncer::spawn(CATALOG_UPDATE_DELAY, move || {
		let realtime = realtime.clone();
		async move {
			let version = chrono::Utc::now().timestamp_millis();
			let connections = realtime.connections.read().await;
			for connection in connections.values() {
				let _ = connection.tx.send(ClientBoundPacket::CatalogUpdated { version });
			}
		}
	})
}

pub(super) fn notify_catalog_updated(state: &ApiState) {
	state.catalog_updates.trigger();
}

/// Matches cosmetics that are not hidden by their group
pub(in crate::api) fn in_enabled_group() -> Condition {
	Condition::any()
		.add(cosmetic::Column::GroupId.is_null())
		.add(
			cosmetic::Column::GroupId.in_subquery(
				Query::select()
					.column(cosmetic_group::Column::Id)
					.from(cosmetic_group::Entity)
					.and_where(cosmetic_group::Column::Enabled.eq(true))
					.to_owned(),
			),
		)
}

/// Matches cosmetics sold under a storefront product and not taken off sale.
/// Variants share their group's product, which an interrupted provision may
/// leave on only some of them, and their group's settings.
pub(super) fn is_sold() -> Condition {
	let off_sale = |column: product_settings::Column| {
		Query::select()
			.column(column)
			.from(product_settings::Entity)
			.and_where(product_settings::Column::ForSale.eq(false))
			.and_where(column.is_not_null())
			.to_owned()
	};

	Condition::all()
		.add(
			Condition::any()
				.add(cosmetic::Column::StoreProductId.is_not_null())
				.add(
					cosmetic::Column::GroupId.in_subquery(
						Query::select()
							.column(cosmetic::Column::GroupId)
							.from(cosmetic::Entity)
							.and_where(cosmetic::Column::StoreProductId.is_not_null())
							.and_where(cosmetic::Column::GroupId.is_not_null())
							.to_owned(),
					),
				),
		)
		.add(
			Condition::any()
				.add(
					Condition::all()
						.add(cosmetic::Column::GroupId.is_null())
						.add(
							cosmetic::Column::Id
								.not_in_subquery(off_sale(product_settings::Column::CosmeticId)),
						),
				)
				.add(cosmetic::Column::GroupId.not_in_subquery(off_sale(
					product_settings::Column::CosmeticGroupId,
				))),
		)
}

pub(super) async fn load_groups<C: ConnectionTrait>(db: &C) -> Result<Groups, DbErr> {
	// Disabled groups are loaded too: a player who owns one of their variants
	// keeps it, so the group is still needed to name and bucket that variant.
	Ok(CosmeticGroup::find()
		.find_with_related(CosmeticGroupAllowedSlot)
		.all(db)
		.await?
		.into_iter()
		.map(|(group, slots)| {
			(
				group.id,
				(group, slots.into_iter().map(|s| s.slot).collect()),
			)
		})
		.collect())
}

/// Fetches every asset in one query, keyed by id.
pub(super) async fn load_assets<C: ConnectionTrait>(
	db: &C,
	cosmetics: impl IntoIterator<Item = &cosmetic::Model>,
) -> Result<HashMap<i32, asset::Model>, DbErr> {
	let ids: Vec<_> = cosmetics
		.into_iter()
		.flat_map(|c| [c.asset_id, c.cover_asset_id])
		.flatten()
		.collect();
	if ids.is_empty() {
		return Ok(HashMap::new());
	}

	Ok(Asset::find()
		.filter(asset::Column::Id.is_in(ids))
		.all(db)
		.await?
		.into_iter()
		.map(|asset| (asset.id, asset))
		.collect())
}

/// Animated preview urls, keyed by preview asset id.
pub(super) async fn preview_urls(
	state: &ApiState,
	cosmetics: impl IntoIterator<Item = &cosmetic::Model>,
) -> Result<HashMap<i32, String>, DbErr> {
	let ids: Vec<i32> = cosmetics
		.into_iter()
		.filter_map(|c| c.preview_asset_id)
		.collect();
	if ids.is_empty() {
		return Ok(HashMap::new());
	}

	Ok(Asset::find()
		.filter(asset::Column::Id.is_in(ids))
		.all(&state.database)
		.await?
		.into_iter()
		.filter_map(|asset| {
			let url = CachedAssetInfo::asset_url(Some(&asset), &state.s3_public_url)?;
			Some((asset.id, url))
		})
		.collect())
}

pub(super) type CosmeticRow = (
	cosmetic::Model,
	Option<asset::Model>,
	Option<asset::Model>,
	Vec<BodySlot>,
);

pub(super) fn is_redundant_variant(cosmetic: &cosmetic::Model) -> bool {
	if cosmetic.r#type == CosmeticType::Nameplate {
		return false;
	}
	let Some(name) = cosmetic.variant_name.as_deref() else {
		return false;
	};

	let lower = name.to_ascii_lowercase();
	lower.contains("reversed") || lower.contains("fold")
}

pub(super) type Groups = HashMap<i32, (cosmetic_group::Model, Vec<BodySlot>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct BucketKey {
	id: i32,
	grouped: bool,
}

impl BucketKey {
	pub(super) fn of(cosmetic: &cosmetic::Model, groups: &Groups) -> Self {
		match cosmetic.group_id.filter(|id| groups.contains_key(id)) {
			Some(id) => Self { id, grouped: true },
			None => Self {
				id: cosmetic.id,
				grouped: false,
			},
		}
	}
}

impl CosmeticInfo {
	pub(super) fn key(&self) -> BucketKey {
		BucketKey {
			id: self.id,
			grouped: self.grouped,
		}
	}
}

pub(super) async fn group_cosmetics(
	cosmetics: Vec<CosmeticRow>,
	groups: Groups,
	cache: Cache<i32, CachedAssetInfo>,
	s3_bucket: Arc<Bucket>,
	public_url: &str,
	hide_redundant_variants: bool,
) -> Result<Vec<CosmeticInfo>, S3Error> {
	let mut variants = Vec::with_capacity(cosmetics.len());
	for (cosmetic, asset, cover_asset, allowed_slots) in cosmetics {
		if hide_redundant_variants && is_redundant_variant(&cosmetic) {
			continue;
		}

		let variant = VariantInfo::from_db_model(
			&cosmetic,
			asset.as_ref(),
			cover_asset.as_ref(),
			allowed_slots.clone(),
			cache.clone(),
			s3_bucket.clone(),
			public_url,
		)
		.await?;
		variants.push((cosmetic, allowed_slots, variant));
	}

	Ok(bucket_variants(variants, &groups))
}

fn bucket_variants(
	variants: Vec<(cosmetic::Model, Vec<BodySlot>, VariantInfo)>,
	groups: &Groups,
) -> Vec<CosmeticInfo> {
	let mut buckets: BTreeMap<BucketKey, (CosmeticInfo, Vec<(i32, VariantInfo)>)> =
		BTreeMap::new();

	for (cosmetic, allowed_slots, variant) in variants {
		let key = BucketKey::of(&cosmetic, groups);
		let entry = match groups.get(&key.id).filter(|_| key.grouped) {
			Some((group, group_slots)) => CosmeticInfo {
				id: key.id,
				grouped: true,
				r#type: group.r#type.clone(),
				name: group.name.clone(),
				allowed_slots: group_slots.clone(),
				variants: Vec::new(),
			},
			None => CosmeticInfo {
				id: cosmetic.id,
				grouped: false,
				r#type: cosmetic.r#type.clone(),
				name: cosmetic
					.name
					.clone()
					.unwrap_or_else(|| format!("Cosmetic {}", cosmetic.id)),
				allowed_slots,
				variants: Vec::new(),
			},
		};

		buckets
			.entry(key)
			.or_insert((entry, Vec::new()))
			.1
			.push((cosmetic.variant_order, variant));
	}

	buckets
		.into_values()
		.map(|(mut entry, mut variants)| {
			variants.sort_by_key(|(order, variant)| (*order, variant.id));
			entry.variants = variants.into_iter().map(|(_, variant)| variant).collect();
			entry
		})
		.collect()
}

impl EmoteInfo {
	#[tracing::instrument(
		name = "convert_db_emote_info",
		level = "debug",
		skip(cache, s3_bucket, public_url)
	)]
	pub async fn from_db_model(
		value: &cosmetic::Model,
		asset: Option<&asset::Model>,
		cache: Cache<i32, CachedAssetInfo>,
		s3_bucket: Arc<Bucket>,
		public_url: &str,
	) -> Result<Self, S3Error> {
		let cached_info =
			CachedAssetInfo::from_optional_asset(asset, cache, s3_bucket).await?;
		Ok(Self {
			id: value.id,
			name: value
				.name
				.clone()
				.unwrap_or_else(|| format!("Emote {}", value.id)),
			url: CachedAssetInfo::asset_url(asset, public_url),
			cached_info,
		})
	}
}

impl CachedAssetInfo {
	const DEFAULT_HASH: &str =
		"74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b";

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

	pub(in crate::api) async fn from_optional_asset(
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

		let info = Self::from_db_model(asset, s3_bucket).await?;
		cache.insert(asset.id, info.clone()).await;

		Ok(info)
	}

	/// The url an asset is served from: its own url when it has one, otherwise
	/// its object served straight off the public bucket.
	pub(in crate::api) fn asset_url(
		asset: Option<&asset::Model>,
		public_url: &str,
	) -> Option<String> {
		let asset = asset?;

		if let Some(url) = &asset.url {
			return Some(url.clone());
		}

		asset
			.storage_path
			.as_ref()
			.map(|path| format!("{public_url}/{path}"))
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

#[cfg(test)]
pub(crate) mod tests {
	use entities::{
		asset, cosmetic, cosmetic_group,
		sea_orm_active_enums::{AssetKind, BodySlot, CosmeticType},
	};

	use super::{
		BucketKey, CachedAssetInfo, Groups, PartialEquippedCosmetics, VariantInfo,
		bucket_variants, is_redundant_variant,
	};
	use crate::utils::hash::sha256_hex;

	pub(crate) fn cosmetic(id: i32, r#type: CosmeticType) -> cosmetic::Model {
		cosmetic::Model {
			id,
			r#type,
			asset_id: None,
			name: Some(format!("Cosmetic {id}")),
			enabled: true,
			created_at: Default::default(),
			updated_at: Default::default(),
			group_id: None,
			variant_name: None,
			model_variant: None,
			variant_order: 0,
			store_product_id: None,
			base_price: None,
			collection: None,
			description: None,
			purchase_count: 0,
			cover_asset_id: None,
			preview_asset_id: None,
			variant_key: None,
		}
	}

	fn row(id: i32, r#type: CosmeticType, group_id: Option<i32>, slot: BodySlot) -> (cosmetic::Model, Vec<BodySlot>, VariantInfo) {
		let cached_info = CachedAssetInfo { hash: CachedAssetInfo::DEFAULT_HASH.to_string() };
		let (name, allowed_slots) = (format!("Variant {id}"), vec![slot.clone()]);
		let variant = VariantInfo { id, name, model: None, allowed_slots, url: None, cover_url: None, variant_key: None, cached_info };
		(cosmetic::Model { group_id, ..cosmetic(id, r#type) }, vec![slot], variant)
	}

	#[test]
	fn groups_and_ungrouped_cosmetics_with_one_id_stay_apart() {
		let cape_group = cosmetic_group::Model {
			id: 20,
			name: "Group 20".to_owned(),
			r#type: CosmeticType::Cape,
			enabled: true,
			created_at: Default::default(),
			updated_at: Default::default(),
		};
		let groups: Groups = [(20, (cape_group, vec![BodySlot::Cape]))].into();
		let rows = vec![
			row(20, CosmeticType::Pet, None, BodySlot::Pet),
			row(31, CosmeticType::Cape, Some(20), BodySlot::Cape),
			row(32, CosmeticType::Cape, Some(20), BodySlot::Cape),
		];
		let infos = bucket_variants(rows, &groups);
		let summary: Vec<_> = infos
			.iter()
			.map(|info| (info.id, info.grouped, info.r#type.clone(), info.name.as_str(), info.variants.iter().map(|v| v.id).collect::<Vec<_>>()))
			.collect();
		assert_eq!(summary, [
			(20, false, CosmeticType::Pet, "Cosmetic 20", vec![20]),
			(20, true, CosmeticType::Cape, "Group 20", vec![31, 32]),
		]);
		assert_ne!(infos[0].key(), infos[1].key());
		let json = serde_json::to_value(&infos).unwrap();
		assert_eq!((&json[0]["grouped"], &json[1]["grouped"]), (&false.into(), &true.into()));

		let unknown_group = cosmetic::Model { group_id: Some(99), ..cosmetic(5, CosmeticType::Pet) };
		assert_eq!(BucketKey::of(&unknown_group, &groups), BucketKey { id: 5, grouped: false });
		let known_group = cosmetic::Model { group_id: Some(20), ..cosmetic(31, CosmeticType::Cape) };
		assert_eq!(BucketKey::of(&known_group, &groups), BucketKey { id: 20, grouped: true });
	}

	#[test]
	fn only_reversed_and_folded_non_nameplate_variants_are_redundant() {
		for (kind, name, redundant) in [
			(CosmeticType::Cape, Some("Reversed"), true),
			(CosmeticType::Cape, Some("Folded"), true),
			(CosmeticType::Cape, Some("Blue"), false),
			(CosmeticType::Cape, None, false),
			(CosmeticType::Nameplate, Some("Reversed"), false),
			(CosmeticType::Nameplate, Some("Fold"), false),
		] {
			let variant = cosmetic::Model { group_id: Some(1), variant_name: name.map(str::to_owned), ..cosmetic(1, kind) };
			assert_eq!(is_redundant_variant(&variant), redundant, "{name:?}");
		}
		let body: PartialEquippedCosmetics = serde_json::from_str(r#"{"equipped":{"effect_a":3,"effect_b":null}}"#).unwrap();
		assert_eq!(body.equipped.get(&BodySlot::EffectA), Some(&Some(3)));
		assert_eq!(body.equipped.get(&BodySlot::EffectB), Some(&None));
	}

	#[test]
	fn default_hash_is_sha256_of_null() {
		assert_eq!(CachedAssetInfo::DEFAULT_HASH, sha256_hex(b"null"));
	}

	fn test_asset(storage_path: Option<&str>, url: Option<&str>) -> asset::Model {
		asset::Model {
			id: 1,
			storage_path: storage_path.map(str::to_owned),
			url: url.map(str::to_owned),
			asset_kind: AssetKind::Image,
			content_type: None,
			hash: None,
			created_at: Default::default(),
			updated_at: Default::default(),
		}
	}

	#[test]
	fn asset_urls_are_plain_public_object_urls() {
		let public_url = "https://objects.example/local";

		assert_eq!(
			CachedAssetInfo::asset_url(
				Some(&test_asset(Some("capes/abc.png"), None)),
				public_url
			),
			Some("https://objects.example/local/capes/abc.png".to_owned())
		);

		// An explicitly stored url wins over object storage.
		assert_eq!(
			CachedAssetInfo::asset_url(
				Some(&test_asset(
					Some("capes/abc.png"),
					Some("https://cdn/x.png")
				)),
				public_url
			),
			Some("https://cdn/x.png".to_owned())
		);

		assert_eq!(
			CachedAssetInfo::asset_url(Some(&test_asset(None, None)), public_url),
			None
		);
		assert_eq!(CachedAssetInfo::asset_url(None, public_url), None);
	}
}
