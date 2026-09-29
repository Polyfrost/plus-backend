use std::collections::{HashMap, HashSet};

use entities::{bundles, bundles_cosmetics, cosmetic, prelude::*};
use sea_orm::{DbErr, prelude::*, sea_query::Query};
use tracing::error;

use crate::{
	api::v0::cosmetics::in_enabled_group, pricing::Sellable, product_settings::Key,
	utils::money::to_cents,
};

/// What a single storefront product id sells.
#[derive(Debug)]
pub(super) enum Product {
	/// Buying one variant grants the whole group, so the line belongs to the
	/// group rather than the variant that was clicked.
	CosmeticGroup {
		group_id: i32,
		cosmetics: Vec<cosmetic::Model>,
	},
	Cosmetic(cosmetic::Model),
	Bundle {
		bundle: bundles::Model,
		cosmetics: Vec<cosmetic::Model>,
	},
}

impl Product {
	pub(super) fn cosmetics(&self) -> &[cosmetic::Model] {
		match self {
			Product::CosmeticGroup { cosmetics, .. }
			| Product::Bundle { cosmetics, .. } => cosmetics,
			Product::Cosmetic(cosmetic) => std::slice::from_ref(cosmetic),
		}
	}

	pub(super) fn key(&self) -> Key {
		match self {
			Product::CosmeticGroup { group_id, .. } => Key::Group(*group_id),
			Product::Cosmetic(cosmetic) => Key::Cosmetic(cosmetic.id),
			Product::Bundle { bundle, .. } => Key::from(bundle),
		}
	}

	pub(super) fn name(&self) -> String {
		match self {
			Product::Bundle { bundle, .. } => bundle.name.clone(),
			_ => self
				.cosmetics()
				.first()
				.and_then(|cosmetic| cosmetic.name.clone())
				.unwrap_or_else(|| format!("{:?}", self.key())),
		}
	}

	/// The list price, in USD minor units, or `None` for a product nothing
	/// priced — which cannot be sold and is rejected before checkout.
	pub(super) fn list_minor(&self) -> Option<i64> {
		let major = match self {
			Product::Bundle { bundle, .. } => bundle.base_price,
			// Variants share one product, so the price is the group's.
			Product::CosmeticGroup { cosmetics, .. } => {
				cosmetics.first().and_then(|cosmetic| cosmetic.base_price)
			}
			Product::Cosmetic(cosmetic) => cosmetic.base_price,
		};

		major.map(to_cents)
	}

	/// How this product looks to the pricing rules.
	pub(super) fn sellable(
		&self,
		product_id: &str,
		tags: &[i32],
		coupons_disabled: bool,
	) -> Sellable {
		let cosmetic_ids = self.cosmetics().iter().map(|c| c.id).collect();

		let (collection, cosmetic_group_id, bundle_id) = match self {
			Product::Bundle { bundle, .. } => (bundle.collection, None, Some(bundle.id)),
			Product::CosmeticGroup { group_id, cosmetics } => (
				cosmetics.first().and_then(|cosmetic| cosmetic.collection),
				Some(*group_id),
				None,
			),
			Product::Cosmetic(cosmetic) => (cosmetic.collection, None, None),
		};

		Sellable {
			product_id: product_id.to_owned(),
			list_minor: self.list_minor().unwrap_or_default(),
			collection,
			tags: tags.to_vec(),
			cosmetic_ids,
			cosmetic_group_id,
			bundle_id,
			coupons_disabled,
		}
	}
}

/// Ids matching nothing are left out, so the caller can reject them by name.
pub(super) async fn resolve_products(
	db: &impl ConnectionTrait,
	product_ids: &[String],
	enabled_only: bool,
) -> Result<HashMap<String, Product>, DbErr> {
	let mut resolved = HashMap::new();
	if product_ids.is_empty() {
		return Ok(resolved);
	}

	let mut cosmetics = Cosmetic::find()
		.filter(cosmetic::Column::StoreProductId.is_in(product_ids.iter().cloned()));
	if enabled_only {
		cosmetics = cosmetics
			.filter(cosmetic::Column::Enabled.eq(true))
			.filter(in_enabled_group());
	}

	let mut by_product: HashMap<String, Vec<cosmetic::Model>> = HashMap::new();
	for cosmetic in cosmetics.all(db).await? {
		let Some(product_id) = cosmetic.store_product_id.clone() else {
			continue;
		};
		by_product.entry(product_id).or_default().push(cosmetic);
	}

	for (product_id, cosmetics) in by_product {
		let group_id = cosmetics.first().and_then(|cosmetic| cosmetic.group_id);
		let product = match group_id {
			// Membership comes from the group, not from who shares the id.
			Some(group_id) => Product::CosmeticGroup {
				group_id,
				cosmetics: Cosmetic::find()
					.filter(cosmetic::Column::GroupId.eq(group_id))
					.all(db)
					.await?,
			},
			None => match cosmetics.into_iter().next() {
				Some(cosmetic) => Product::Cosmetic(cosmetic),
				None => continue,
			},
		};
		resolved.insert(product_id, product);
	}

	let mut bundles = Bundles::find()
		.filter(bundles::Column::StoreProductId.is_in(product_ids.iter().cloned()));
	if enabled_only {
		bundles = bundles.filter(bundles::Column::Enabled.eq(true));
	}

	for bundle in bundles.all(db).await? {
		let Some(product_id) = bundle.store_product_id.clone() else {
			continue;
		};

		// Nothing enforces this across the two tables, and silently picking
		// one would charge for whichever the map happened to keep.
		if resolved.contains_key(&product_id) {
			error!(
				product = %product_id,
				bundle = bundle.id,
				"Product id claimed by both a cosmetic and a bundle; ignoring the bundle"
			);
			continue;
		}

		let cosmetics = Cosmetic::find()
			.filter(
				cosmetic::Column::Id.in_subquery(
					Query::select()
						.column(bundles_cosmetics::Column::CosmeticId)
						.from(bundles_cosmetics::Entity)
						.and_where(bundles_cosmetics::Column::BundleId.eq(bundle.id))
						.to_owned(),
				),
			)
			.all(db)
			.await?;

		resolved.insert(
			product_id,
			Product::Bundle { bundle, cosmetics },
		);
	}

	Ok(resolved)
}

/// Keeps the buyer's order. You cannot own two of the same cosmetic.
pub(super) fn dedupe(product_ids: Vec<String>) -> Vec<String> {
	let mut seen = HashSet::new();
	product_ids
		.into_iter()
		.filter(|product_id| seen.insert(product_id.clone()))
		.collect()
}

pub(super) fn display_name(cosmetic: &cosmetic::Model) -> String {
	let base = cosmetic
		.name
		.clone()
		.unwrap_or_else(|| format!("Cosmetic #{}", cosmetic.id));

	match &cosmetic.variant_name {
		Some(variant) => format!("{base} ({variant})"),
		None => base,
	}
}
