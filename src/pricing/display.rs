use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, Utc};
use entities::{bundles, cosmetic, prelude::*, tags_cosmetic};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait as _, DbErr, EntityTrait, QueryFilter as _, prelude::*};
use serde::Serialize;

use super::{Sellable, live_rules, quote};
use crate::{
	product_settings::{Key, SettingsInfo},
	utils::money::to_cents,
};

/// The sale a storefront entry is currently in, ready to render next to the
/// struck-through list price.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SaleInfo {
	/// What the buyer pays, in USD major units.
	pub price: f32,
	/// What comes off the list price, in USD major units.
	pub discount: f32,
	/// Whole percent off, rounded, for a "-25%" badge.
	pub percent: i32,
	/// Joined with " + " when several stack.
	pub name: String,
	/// When the sale stops, as an RFC 3339 timestamp. Null runs until pulled.
	pub ends_at: Option<String>,
}

/// Prices every cosmetic in `cosmetics` against the sales running right now.
///
/// Entries with no sale are left out, so a caller can treat a miss as "list
/// price" without allocating anything per row.
pub(crate) async fn cosmetic_sales(
	db: &impl ConnectionTrait,
	cosmetics: &[cosmetic::Model],
	settings: &HashMap<Key, SettingsInfo>,
) -> Result<HashMap<i32, SaleInfo>, DbErr> {
	let priced: Vec<&cosmetic::Model> = cosmetics
		.iter()
		.filter(|cosmetic| cosmetic.base_price.is_some_and(|price| price > 0.0))
		.collect();
	if priced.is_empty() {
		return Ok(HashMap::new());
	}

	let rules = live_rules(db, &[], None, Utc::now().into()).await?;
	if rules.is_empty() {
		return Ok(HashMap::new());
	}

	let tags = tags_by_cosmetic(db, priced.iter().map(|row| row.id)).await?;
	let lines: Vec<Sellable> = priced
		.iter()
		.map(|row| Sellable {
			// Keyed by the row, not the storefront product: variants of one
			// group share a product id and would collide.
			product_id: row.id.to_string(),
			list_minor: to_cents(row.base_price.unwrap_or_default()),
			collection: row.collection,
			tags: tags.get(&row.id).cloned().unwrap_or_default(),
			cosmetic_ids: vec![row.id],
			cosmetic_group_id: row.group_id,
			bundle_id: None,
			coupons_disabled: coupons_disabled(settings, Key::of_cosmetic(row)),
		})
		.collect();

	Ok(collect(quote(&lines, &rule_list(&rules), "usd"), &rules))
}

pub(crate) async fn bundle_sales(
	db: &impl ConnectionTrait,
	bundles: &[bundles::Model],
	settings: &HashMap<Key, SettingsInfo>,
) -> Result<HashMap<i32, SaleInfo>, DbErr> {
	let priced: Vec<&bundles::Model> = bundles
		.iter()
		.filter(|bundle| bundle.base_price.is_some_and(|price| price > 0.0))
		.collect();
	if priced.is_empty() {
		return Ok(HashMap::new());
	}

	let rules = live_rules(db, &[], None, Utc::now().into()).await?;
	if rules.is_empty() {
		return Ok(HashMap::new());
	}

	let lines: Vec<Sellable> = priced
		.iter()
		.map(|row| Sellable {
			product_id: row.id.to_string(),
			list_minor: to_cents(row.base_price.unwrap_or_default()),
			collection: row.collection,
			bundle_id: Some(row.id),
			coupons_disabled: coupons_disabled(settings, Key::from(*row)),
			..Default::default()
		})
		.collect();

	Ok(collect(quote(&lines, &rule_list(&rules), "usd"), &rules))
}

fn coupons_disabled(settings: &HashMap<Key, SettingsInfo>, key: Key) -> bool {
	settings.get(&key).is_some_and(|info| info.coupons_disabled)
}

fn rule_list(rules: &[super::LiveDiscount]) -> Vec<super::Rule> {
	rules.iter().map(|live| live.rule.clone()).collect()
}

/// Turns the quote back into per-row sale info, keyed by the id the caller
/// passed in as the product id.
fn collect(
	quoted: super::Quote,
	rules: &[super::LiveDiscount],
) -> HashMap<i32, SaleInfo> {
	let ends_at: HashMap<i32, Option<DateTime<FixedOffset>>> = rules
		.iter()
		.map(|live| (live.model.id, live.model.ends_at))
		.collect();

	quoted
		.lines
		.into_iter()
		.filter_map(|line| {
			if line.applied.is_empty() {
				return None;
			}
			let id = line.product_id.parse().ok()?;

			Some((
				id,
				SaleInfo {
					price: minor_to_major(line.total_minor),
					discount: minor_to_major(line.discount_minor),
					percent: percent_off(line.list_minor, line.discount_minor),
					name: line
						.applied
						.iter()
						.map(|applied| applied.name.as_str())
						.collect::<Vec<_>>()
						.join(" + "),
					// The first to end is when this price stops.
					ends_at: line
						.applied
						.iter()
						.filter_map(|applied| ends_at.get(&applied.discount_id).copied().flatten())
						.min()
						.map(|at| at.to_rfc3339()),
				},
			))
		})
		.collect()
}

#[expect(
	clippy::cast_precision_loss,
	reason = "catalogue prices are far below the f32 mantissa"
)]
fn minor_to_major(minor: i64) -> f32 {
	minor as f32 / 100.0
}

fn percent_off(list_minor: i64, discount_minor: i64) -> i32 {
	if list_minor <= 0 {
		return 0;
	}

	i32::try_from((discount_minor * 100 + list_minor / 2) / list_minor).unwrap_or(0)
}

async fn tags_by_cosmetic(
	db: &impl ConnectionTrait,
	ids: impl Iterator<Item = i32>,
) -> Result<HashMap<i32, Vec<i32>>, DbErr> {
	let mut tags: HashMap<i32, Vec<i32>> = HashMap::new();
	for row in TagsCosmetic::find()
		.filter(tags_cosmetic::Column::CosmeticId.is_in(ids.collect::<Vec<_>>()))
		.all(db)
		.await?
	{
		tags.entry(row.cosmetic_id).or_default().push(row.tag_id);
	}

	Ok(tags)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_percentage_badge_rounds_to_a_whole_percent() {
		assert_eq!(percent_off(1000, 250), 25);
		// 499 - 50 off is 10.02%, which reads as 10.
		assert_eq!(percent_off(499, 50), 10);
		assert_eq!(percent_off(0, 0), 0);
	}

	#[test]
	fn minor_units_come_back_as_major() {
		assert!((minor_to_major(750) - 7.5).abs() < f32::EPSILON);
	}
}
