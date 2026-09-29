//! Keeps the PayNow storefront in step with the catalogue and discounts the
//! backend owns. Every change flows one way, from here to PayNow.
//!
//! Tags and collections are mirrored as PayNow tags, so a sale scoped to
//! "everything tagged winter" or "the summer collection" is one rule there
//! too, and covers whatever joins it later.

use std::collections::{HashMap, HashSet};

use entities::{bundles, collections, cosmetic, discount, prelude::*, tags, tags_cosmetic};
use sea_orm::{
	ActiveModelTrait as _, ColumnTrait as _, DbErr, EntityTrait, QueryFilter as _, Set,
	prelude::*,
};
use tracing::{debug, warn};

use crate::{
	paynow::{
		PayNowClient, PayNowError, catalog,
		models::{UpsertCoupon, UpsertProduct, UpsertSale},
		promotions::Promotion,
	},
	pricing::{Amount, Targets, amount_of},
	product_settings::Key,
};

/// Prices are stored in USD; `provision-paynow` refuses any other store.
const STORE_CURRENCY: &str = "usd";
/// PayNow takes a percentage times ten: 250 is 25%.
const SALE_PERCENT_SCALE: i64 = 10;
/// Undocumented for coupons; assumed the same as sales until a real coupon
/// shows otherwise.
const COUPON_PERCENT_SCALE: i64 = 10;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SyncError {
	#[error("{0}")]
	Database(#[from] DbErr),
	#[error("{0}")]
	PayNow(#[from] PayNowError),
	#[error(
		"None of its targets are on PayNow yet, and an empty scope there means \
		 everything. Provision them, then run provision-paynow --sync-discounts."
	)]
	Untargetable,
}

/// Maps local tag ids to their storefront ids, creating any that are missing.
///
/// An existing storefront tag with the same slug is adopted rather than
/// duplicated, so a crash between creating one and writing its id back is
/// repaired by the next run.
pub(crate) async fn ensure_tags(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	tag_ids: &[i32],
) -> Result<HashMap<i32, String>, SyncError> {
	if tag_ids.is_empty() {
		return Ok(HashMap::new());
	}

	let unique: HashSet<i32> = tag_ids.iter().copied().collect();
	let rows = Tags::find()
		.filter(tags::Column::Id.is_in(unique))
		.all(db)
		.await?;

	let mut mapped = HashMap::with_capacity(rows.len());
	let mut missing = Vec::new();
	for row in rows {
		match &row.paynow_tag_id {
			Some(paynow_id) => {
				mapped.insert(row.id, paynow_id.clone());
			}
			None => missing.push(row),
		}
	}

	if missing.is_empty() {
		return Ok(mapped);
	}

	// One listing covers every tag that still needs adopting.
	let existing: HashMap<String, String> = client
		.tags()
		.await?
		.into_iter()
		.map(|tag| (tag.slug, tag.id))
		.collect();

	for row in missing {
		let slug = catalog::tag_slug(row.id);
		let paynow_id = match existing.get(&slug) {
			Some(id) => {
				debug!(tag = row.id, slug, "Adopting existing storefront tag");
				id.clone()
			}
			None => {
				client
					.create_tag(
						&slug,
						row.display_name.as_deref().unwrap_or(&row.name),
						row.description.as_deref(),
					)
					.await?
					.id
			}
		};

		let id = row.id;
		let mut active: tags::ActiveModel = row.into();
		active.paynow_tag_id = Set(Some(paynow_id.clone()));
		active.update(db).await?;

		mapped.insert(id, paynow_id);
	}

	Ok(mapped)
}

/// Maps collection ids to the storefront tags standing in for them,
/// adopting by slug and creating any that are missing.
async fn collection_tags(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	collection_ids: &[i32],
) -> Result<HashMap<i32, String>, SyncError> {
	if collection_ids.is_empty() {
		return Ok(HashMap::new());
	}

	let existing: HashMap<String, String> = client
		.tags()
		.await?
		.into_iter()
		.map(|tag| (tag.slug, tag.id))
		.collect();

	let mut mapped = HashMap::with_capacity(collection_ids.len());
	for collection in Collections::find()
		.filter(collections::Column::Id.is_in(collection_ids.to_vec()))
		.all(db)
		.await?
	{
		let slug = catalog::collection_slug(collection.id);
		let id = match existing.get(&slug) {
			Some(id) => id.clone(),
			None => {
				client
					.create_tag(&slug, &collection.name, collection.description.as_deref())
					.await?
					.id
			}
		};
		mapped.insert(collection.id, id);
	}

	Ok(mapped)
}

/// The local tags and collections one storefront product should carry.
#[derive(Debug, Default)]
struct Wanted {
	tags: HashSet<i32>,
	collections: HashSet<i32>,
}

/// Pushes each product's whole tag set. Returns how many were pushed.
async fn push_tag_sets(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	products: HashMap<String, Wanted>,
) -> Result<usize, SyncError> {
	let tag_ids: Vec<i32> = products
		.values()
		.flat_map(|wanted| wanted.tags.iter().copied())
		.collect();
	let collection_ids: Vec<i32> = products
		.values()
		.flat_map(|wanted| wanted.collections.iter().copied())
		.collect::<HashSet<_>>()
		.into_iter()
		.collect();
	let tags = ensure_tags(db, client, &tag_ids).await?;
	let collections = collection_tags(db, client, &collection_ids).await?;

	let pushed = products.len();
	for (product_id, wanted) in products {
		let mut ids: Vec<String> = wanted
			.tags
			.iter()
			.filter_map(|id| tags.get(id).cloned())
			.chain(
				wanted
					.collections
					.iter()
					.filter_map(|id| collections.get(id).cloned()),
			)
			.collect();
		// Stable so an unchanged set is an identical request.
		ids.sort_unstable();

		client
			.update_product(
				&product_id,
				&UpsertProduct {
					tags: Some(ids),
					..Default::default()
				},
			)
			.await?;
	}

	Ok(pushed)
}

/// Pushes the storefront tag set of every product these cosmetics are sold
/// under.
///
/// Variants share one product, so the set is the union across the whole group:
/// tagging one variant tags the thing the buyer actually sees.
/// Returns how many products were pushed.
pub(crate) async fn sync_cosmetic_tags(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	cosmetic_ids: &[i32],
) -> Result<usize, SyncError> {
	if cosmetic_ids.is_empty() {
		return Ok(0);
	}

	let products = products_for(db, cosmetic_ids).await?;
	push_tag_sets(db, client, products).await
}

/// A bundle carries only its collection.
pub(crate) async fn sync_bundle_tags(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	bundle_ids: &[i32],
) -> Result<usize, SyncError> {
	if bundle_ids.is_empty() {
		return Ok(0);
	}

	let products = Bundles::find()
		.filter(bundles::Column::Id.is_in(bundle_ids.to_vec()))
		.filter(bundles::Column::StoreProductId.is_not_null())
		.all(db)
		.await?
		.into_iter()
		.filter_map(|bundle| {
			let wanted = Wanted {
				tags: HashSet::new(),
				collections: bundle.collection.into_iter().collect(),
			};
			Some((bundle.store_product_id?, wanted))
		})
		.collect();
	push_tag_sets(db, client, products).await
}

/// The same, but a failure only warns: used after a database commit that the
/// caller is not willing to fail over storefront drift.
pub(crate) async fn sync_cosmetic_tags_or_warn(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	cosmetic_ids: &[i32],
) {
	if let Err(error) = sync_cosmetic_tags(db, client, cosmetic_ids).await {
		warn!(
			cosmetics = cosmetic_ids.len(),
			"Unable to push tags to the storefront; \
			 provision-paynow --sync-tags will repair it: {error}"
		);
	}
}

pub(crate) async fn sync_bundle_tags_or_warn(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	bundle_ids: &[i32],
) {
	if let Err(error) = sync_bundle_tags(db, client, bundle_ids).await {
		warn!(
			bundles = bundle_ids.len(),
			"Unable to push tags to the storefront; \
			 provision-paynow --sync-tags will repair it: {error}"
		);
	}
}

/// Storefront product id to what that product should carry.
async fn products_for(
	db: &impl ConnectionTrait,
	cosmetic_ids: &[i32],
) -> Result<HashMap<String, Wanted>, DbErr> {
	let touched = Cosmetic::find()
		.filter(cosmetic::Column::Id.is_in(cosmetic_ids.to_vec()))
		.all(db)
		.await?;

	// A grouped cosmetic is sold as its group, so the whole group's tags
	// decide what the product carries.
	let group_ids: Vec<i32> = touched.iter().filter_map(|row| row.group_id).collect();
	let mut members = touched;
	if !group_ids.is_empty() {
		let siblings = Cosmetic::find()
			.filter(cosmetic::Column::GroupId.is_in(group_ids))
			.all(db)
			.await?;
		let known: HashSet<i32> = members.iter().map(|row| row.id).collect();
		members.extend(
			siblings
				.into_iter()
				.filter(|row| !known.contains(&row.id)),
		);
	}

	let product_of = product_ids(&members);
	if product_of.is_empty() {
		return Ok(HashMap::new());
	}

	// Every product is listed, even with nothing to carry: a product whose last
	// tag was just removed still needs the empty set pushed.
	let mut by_product: HashMap<String, Wanted> = HashMap::new();
	for member in &members {
		let Some(product_id) = product_of.get(&member.id) else {
			continue;
		};
		let wanted = by_product.entry(product_id.clone()).or_default();
		wanted.collections.extend(member.collection);
	}
	for (cosmetic_id, tag_id) in tag_links(db, &members).await? {
		if let Some(wanted) = product_of
			.get(&cosmetic_id)
			.and_then(|product_id| by_product.get_mut(product_id))
		{
			wanted.tags.insert(tag_id);
		}
	}

	Ok(by_product)
}

/// The storefront product each cosmetic is sold under, skipping any that has
/// not been provisioned yet.
fn product_ids(members: &[cosmetic::Model]) -> HashMap<i32, String> {
	// Variants share one id, and an interrupted provision can leave it on only
	// some of them, so the group's id is whichever member has one.
	let mut by_group: HashMap<i32, String> = HashMap::new();
	for row in members {
		if let (Some(group_id), Some(product_id)) = (row.group_id, &row.store_product_id)
		{
			by_group.entry(group_id).or_insert_with(|| product_id.clone());
		}
	}

	members
		.iter()
		.filter_map(|row| {
			let product_id = match row.group_id {
				Some(group_id) => by_group.get(&group_id).cloned(),
				None => row.store_product_id.clone(),
			}?;

			Some((row.id, product_id))
		})
		.collect()
}

async fn tag_links(
	db: &impl ConnectionTrait,
	members: &[cosmetic::Model],
) -> Result<Vec<(i32, i32)>, DbErr> {
	let ids: Vec<i32> = members.iter().map(|row| row.id).collect();
	if ids.is_empty() {
		return Ok(Vec::new());
	}

	Ok(TagsCosmetic::find()
		.filter(tags_cosmetic::Column::CosmeticId.is_in(ids))
		.all(db)
		.await?
		.into_iter()
		.map(|link| (link.cosmetic_id, link.tag_id))
		.collect())
}

/// Which PayNow promotion a discount is: a coupon when it has a code.
pub(crate) fn promotion_of(model: &discount::Model) -> Promotion {
	if model.code.is_some() {
		Promotion::Coupon
	} else {
		Promotion::Sale
	}
}

/// Creates or replaces the PayNow copy of a discount, returning the id to
/// store on it. `None` when PayNow cannot carry it: a fixed amount in a
/// currency it does not sell in, such as a future in-game balance.
pub(crate) async fn push_discount(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	previous: Option<&discount::Model>,
	model: &discount::Model,
	targets: &Targets,
) -> Result<Option<String>, SyncError> {
	let kind = promotion_of(model);

	// A sale that became a coupon, or back, is a different object there.
	let mut existing = previous
		.and_then(|previous| Some((promotion_of(previous), previous.paynow_id.clone()?)));
	if let Some((old_kind, id)) = &existing
		&& *old_kind != kind
	{
		client.delete_promotion(*old_kind, id).await?;
		existing = None;
	}

	let (discount_type, discount_amount) = match amount_of(model) {
		Some(Amount::Percent(percent)) => (
			"percent",
			i64::from(percent)
				* match kind {
					Promotion::Sale => SALE_PERCENT_SCALE,
					Promotion::Coupon => COUPON_PERCENT_SCALE,
				},
		),
		Some(Amount::Fixed { minor, currency })
			if currency.eq_ignore_ascii_case(STORE_CURRENCY) =>
		{
			("amount", minor)
		}
		_ => {
			if let Some((kind, id)) = existing {
				client.delete_promotion(kind, &id).await?;
			}
			return Ok(None);
		}
	};

	// Nothing left to cover, say after its only collection was deleted.
	if !model.applies_to_all && targets.is_empty() {
		if let Some((kind, id)) = existing {
			client.delete_promotion(kind, &id).await?;
		}
		return Ok(None);
	}

	let (products, tags) = if model.applies_to_all {
		(Vec::new(), Vec::new())
	} else {
		let scope = paynow_scope(db, client, targets).await?;
		if scope.0.is_empty() && scope.1.is_empty() {
			return Err(SyncError::Untargetable);
		}
		scope
	};

	let at = |time: Option<DateTimeWithTimeZone>| time.map(|at| at.to_rfc3339());
	let minimum_order_value = model.min_subtotal_minor.unwrap_or(0);

	let pushed = match kind {
		Promotion::Sale => {
			let body = UpsertSale {
				name: &model.name,
				enabled: model.enabled,
				discount_type,
				discount_amount,
				duration: "once",
				minimum_order_value,
				apply_to_product_ids: &products,
				apply_to_tag_ids: &tags,
				begins_at: model.starts_at.unwrap_or(model.created_at).to_rfc3339(),
				ends_at: at(model.ends_at),
			};
			upsert(client, kind, existing.map(|(_, id)| id), &body).await?
		}
		Promotion::Coupon => {
			let body = UpsertCoupon {
				code: model.code.as_deref().unwrap_or_default(),
				note: model.description.as_deref(),
				enabled: model.enabled,
				discount_type,
				discount_amount,
				// Per item and after the sale, the way the backend quotes it.
				discount_apply_individually: true,
				discount_apply_before_sales: false,
				duration: "once",
				minimum_order_value,
				apply_to_products: &products,
				apply_to_tags: &tags,
				redeem_limit_store_enabled: model.max_redemptions.is_some(),
				redeem_limit_store_amount: model.max_redemptions.unwrap_or(0),
				redeem_limit_customer_enabled: model.max_per_player.is_some(),
				redeem_limit_customer_amount: model.max_per_player.unwrap_or(0),
				usable_on_one_time_purchase: true,
				usable_on_subscription: false,
				usable_at: at(model.starts_at),
				expires_at: at(model.ends_at),
			};
			upsert(client, kind, existing.map(|(_, id)| id), &body).await?
		}
	};

	Ok(Some(pushed))
}

/// `push_discount`, then stores the id it comes back with.
pub(crate) async fn push_and_store(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	previous: Option<&discount::Model>,
	model: discount::Model,
	targets: &Targets,
) -> Result<discount::Model, SyncError> {
	let paynow_id = push_discount(db, client, previous, &model, targets).await?;
	if paynow_id == model.paynow_id {
		return Ok(model);
	}

	let mut active: discount::ActiveModel = model.into();
	active.paynow_id = Set(paynow_id);
	Ok(active.update(db).await?)
}

/// Re-pushes the discounts that targeted a just-deleted collection, then
/// deletes the PayNow tag that stood in for it.
pub(crate) async fn collection_deleted(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	collection_id: i32,
	discount_ids: &[i32],
) -> Result<(), SyncError> {
	let discounts = Discount::find()
		.filter(discount::Column::Id.is_in(discount_ids.to_vec()))
		.all(db)
		.await?;
	let mut targets = crate::pricing::targets_by_discount(db, &discounts).await?;
	for discount in discounts {
		let own = targets.remove(&discount.id).unwrap_or_default();
		let previous = discount.clone();
		push_and_store(db, client, Some(&previous), discount, &own).await?;
	}

	let slug = catalog::collection_slug(collection_id);
	if let Some(tag) = client.tags().await?.into_iter().find(|tag| tag.slug == slug) {
		client.delete_tag(&tag.id).await?;
	}

	Ok(())
}

async fn upsert(
	client: &PayNowClient,
	kind: Promotion,
	existing: Option<String>,
	body: &impl serde::Serialize,
) -> Result<String, PayNowError> {
	match existing {
		Some(id) => {
			client.update_promotion(kind, &id, body).await?;
			Ok(id)
		}
		None => client.create_promotion(kind, body).await,
	}
}

/// The PayNow products and tags a discount's targets come to. Anything not
/// provisioned yet is left out.
async fn paynow_scope(
	db: &impl ConnectionTrait,
	client: &PayNowClient,
	targets: &Targets,
) -> Result<(Vec<String>, Vec<String>), SyncError> {
	let mut keys: Vec<Key> = Cosmetic::find()
		.filter(cosmetic::Column::Id.is_in(targets.cosmetics.clone()))
		.all(db)
		.await?
		.iter()
		.map(Key::of_cosmetic)
		.collect();
	keys.extend(targets.cosmetic_groups.iter().copied().map(Key::Group));
	keys.extend(targets.bundles.iter().copied().map(Key::Bundle));

	let mut products = HashSet::new();
	for key in keys {
		match key.product_id(db).await? {
			Some(product_id) => {
				products.insert(product_id);
			}
			None => warn!(?key, "Discount target is not on PayNow yet; leaving it out"),
		}
	}

	let mut tags: HashSet<String> =
		ensure_tags(db, client, &targets.tags).await?.into_values().collect();
	tags.extend(
		collection_tags(db, client, &targets.collections)
			.await?
			.into_values(),
	);

	let mut products: Vec<String> = products.into_iter().collect();
	let mut tags: Vec<String> = tags.into_iter().collect();
	products.sort_unstable();
	tags.sort_unstable();
	Ok((products, tags))
}

pub(crate) async fn delete_discount(
	client: &PayNowClient,
	model: &discount::Model,
) -> Result<(), PayNowError> {
	match &model.paynow_id {
		Some(id) => client.delete_promotion(promotion_of(model), id).await,
		None => Ok(()),
	}
}

#[cfg(test)]
mod tests {
	use chrono::Utc;
	use entities::sea_orm_active_enums::CosmeticType;

	use super::*;

	fn cosmetic(
		id: i32,
		group_id: Option<i32>,
		store_product_id: Option<&str>,
	) -> cosmetic::Model {
		cosmetic::Model {
			id,
			r#type: CosmeticType::Cape,
			asset_id: None,
			name: None,
			enabled: true,
			created_at: Utc::now().into(),
			updated_at: Utc::now().into(),
			group_id,
			variant_name: None,
			model_variant: None,
			variant_order: 0,
			store_product_id: store_product_id.map(str::to_owned),
			base_price: None,
			collection: None,
			description: None,
			purchase_count: 0,
			cover_asset_id: None,
		}
	}

	#[test]
	fn an_ungrouped_cosmetic_maps_to_its_own_product() {
		let members = [cosmetic(1, None, Some("p1"))];
		let map = product_ids(&members);

		assert_eq!(map.get(&1).map(String::as_str), Some("p1"));
	}

	#[test]
	fn variants_share_the_product_id_one_of_them_carries() {
		// An interrupted provision can leave the id on a single variant.
		let members = [
			cosmetic(1, Some(7), None),
			cosmetic(2, Some(7), Some("p7")),
			cosmetic(3, Some(7), None),
		];
		let map = product_ids(&members);

		for id in [1, 2, 3] {
			assert_eq!(
				map.get(&id).map(String::as_str),
				Some("p7"),
				"variant {id} should resolve to the group's product"
			);
		}
	}

	#[test]
	fn a_cosmetic_with_no_product_anywhere_is_skipped() {
		let members = [cosmetic(1, None, None), cosmetic(2, Some(7), None)];
		assert!(product_ids(&members).is_empty());
	}
}
