//! Per-product rules the backend enforces at checkout: when it is on sale,
//! how many can be sold, who can buy it and whether discounts apply.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use entities::{
	bundles, cosmetic, prelude::*, product_requirement, product_settings,
	sea_orm_active_enums::TransactionStatus, transaction, transaction_line,
};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, Condition, DbErr, EntityTrait, FromQueryResult, JoinType,
	PaginatorTrait as _, QueryFilter as _, QuerySelect as _, RelationTrait as _,
	prelude::*,
};
use serde::Serialize;

use crate::paynow::{PayNowClient, PayNowError, models::UpsertProduct};

/// Whichever row is sold as the product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Key {
	Cosmetic(i32),
	Group(i32),
	Bundle(i32),
}

impl Key {
	/// A grouped cosmetic is sold as its group.
	pub(crate) fn of_cosmetic(cosmetic: &cosmetic::Model) -> Self {
		cosmetic
			.group_id
			.map_or(Self::Cosmetic(cosmetic.id), Self::Group)
	}

	pub(crate) fn of_row(row: &product_settings::Model) -> Option<Self> {
		row.cosmetic_id
			.map(Self::Cosmetic)
			.or(row.cosmetic_group_id.map(Self::Group))
			.or(row.bundle_id.map(Self::Bundle))
	}

	fn of_line(line: &SoldCount) -> Option<Self> {
		line.cosmetic_id
			.map(Self::Cosmetic)
			.or(line.cosmetic_group_id.map(Self::Group))
			.or(line.bundle_id.map(Self::Bundle))
	}

	pub(crate) fn settings_filter(self) -> Condition {
		Condition::all().add(match self {
			Self::Cosmetic(id) => product_settings::Column::CosmeticId.eq(id),
			Self::Group(id) => product_settings::Column::CosmeticGroupId.eq(id),
			Self::Bundle(id) => product_settings::Column::BundleId.eq(id),
		})
	}

	fn line_filter(self) -> Condition {
		Condition::all().add(match self {
			Self::Cosmetic(id) => transaction_line::Column::CosmeticId.eq(id),
			Self::Group(id) => transaction_line::Column::CosmeticGroupId.eq(id),
			Self::Bundle(id) => transaction_line::Column::BundleId.eq(id),
		})
	}

	/// The storefront product this is sold under, once provisioned.
	pub(crate) async fn product_id(
		self,
		db: &impl ConnectionTrait,
	) -> Result<Option<String>, DbErr> {
		Ok(match self {
			Self::Cosmetic(id) => Cosmetic::find_by_id(id)
				.one(db)
				.await?
				.and_then(|row| row.store_product_id),
			// An interrupted provision can leave the id on only some variants.
			Self::Group(id) => Cosmetic::find()
				.filter(cosmetic::Column::GroupId.eq(id))
				.filter(cosmetic::Column::StoreProductId.is_not_null())
				.one(db)
				.await?
				.and_then(|row| row.store_product_id),
			Self::Bundle(id) => Bundles::find_by_id(id)
				.one(db)
				.await?
				.and_then(|row| row.store_product_id),
		})
	}
}

impl From<&bundles::Model> for Key {
	fn from(bundle: &bundles::Model) -> Self {
		Self::Bundle(bundle.id)
	}
}

#[derive(Debug, Clone)]
pub(crate) struct Settings {
	pub model: product_settings::Model,
	pub requirements: Vec<i32>,
}

impl Settings {
	pub(crate) fn is_available(&self, now: DateTime<FixedOffset>) -> bool {
		self.model.for_sale
			&& self.model.available_from.is_none_or(|from| now >= from)
			&& self.model.available_until.is_none_or(|until| now < until)
	}

	pub(crate) fn requirements_met(&self, owned: &HashSet<i32>) -> bool {
		if self.model.requires_all {
			self.requirements.iter().all(|id| owned.contains(id))
		} else {
			self.requirements.is_empty()
				|| self.requirements.iter().any(|id| owned.contains(id))
		}
	}
}

pub(crate) async fn load(
	db: &impl ConnectionTrait,
	keys: &[Key],
) -> Result<HashMap<Key, Settings>, DbErr> {
	if keys.is_empty() {
		return Ok(HashMap::new());
	}

	let rows = ProductSettings::find()
		.filter(
			keys.iter()
				.fold(Condition::any(), |any, key| any.add(key.settings_filter())),
		)
		.all(db)
		.await?;
	if rows.is_empty() {
		return Ok(HashMap::new());
	}

	let mut requirements: HashMap<i32, Vec<i32>> = HashMap::new();
	for row in ProductRequirement::find()
		.filter(
			product_requirement::Column::SettingsId.is_in(rows.iter().map(|row| row.id)),
		)
		.all(db)
		.await?
	{
		requirements
			.entry(row.settings_id)
			.or_default()
			.push(row.cosmetic_id);
	}

	Ok(rows
		.into_iter()
		.filter_map(|model| {
			let key = Key::of_row(&model)?;
			let requirements = requirements.remove(&model.id).unwrap_or_default();
			Some((key, Settings { model, requirements }))
		})
		.collect())
}

#[derive(Debug, FromQueryResult)]
struct SoldCount {
	cosmetic_id: Option<i32>,
	cosmetic_group_id: Option<i32>,
	bundle_id: Option<i32>,
	sold: i64,
}

/// Units sold and not returned, for the keys that carry a stock limit.
pub(crate) async fn sold(
	db: &impl ConnectionTrait,
	settings: &HashMap<Key, Settings>,
) -> Result<HashMap<Key, i64>, DbErr> {
	let limited: Vec<Key> = settings
		.iter()
		.filter(|(_, settings)| settings.model.stock_limit.is_some())
		.map(|(key, _)| *key)
		.collect();
	if limited.is_empty() {
		return Ok(HashMap::new());
	}

	Ok(TransactionLine::find()
		.select_only()
		.column(transaction_line::Column::CosmeticId)
		.column(transaction_line::Column::CosmeticGroupId)
		.column(transaction_line::Column::BundleId)
		.column_as(transaction_line::Column::Id.count(), "sold")
		.filter(transaction_line::Column::Status.eq(TransactionStatus::Completed))
		.filter(
			limited
				.iter()
				.fold(Condition::any(), |any, key| any.add(key.line_filter())),
		)
		.group_by(transaction_line::Column::CosmeticId)
		.group_by(transaction_line::Column::CosmeticGroupId)
		.group_by(transaction_line::Column::BundleId)
		.into_model::<SoldCount>()
		.all(db)
		.await?
		.into_iter()
		.filter_map(|count| Some((Key::of_line(&count)?, count.sold)))
		.collect())
}

/// Units of `key` this player has paid for, gifts included, within the
/// settings' window.
pub(crate) async fn bought_by(
	db: &impl ConnectionTrait,
	key: Key,
	settings: &product_settings::Model,
	buyer_id: i32,
) -> Result<u64, DbErr> {
	let mut query = TransactionLine::find()
		.join(JoinType::InnerJoin, transaction_line::Relation::Transaction.def())
		.filter(key.line_filter())
		.filter(transaction_line::Column::Status.eq(TransactionStatus::Completed))
		.filter(
			Condition::any()
				.add(transaction::Column::Buyer.eq(buyer_id))
				.add(
					Condition::all()
						.add(transaction::Column::Buyer.is_null())
						.add(transaction::Column::PlayerId.eq(buyer_id)),
				),
		);
	if let Some(days) = settings.customer_limit_days {
		query = query.filter(
			transaction_line::Column::CreatedAt.gte(Utc::now() - Duration::days(days.into())),
		);
	}

	query.count(db).await
}

/// What the client needs to show a product as buyable or not.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SettingsInfo {
	/// False when taken off sale by hand, whatever the window says.
	pub for_sale: bool,
	pub available_from: Option<DateTime<FixedOffset>>,
	pub available_until: Option<DateTime<FixedOffset>>,
	/// Units that can ever be sold, refunds excluded.
	pub stock_limit: Option<i32>,
	/// Units left, when stock is limited. Zero is sold out.
	pub stock_remaining: Option<i64>,
	/// How many one player may buy, gifts included.
	pub customer_limit: Option<i32>,
	/// The window `customer_limit` counts over. Null is forever.
	pub customer_limit_days: Option<i32>,
	pub gifting_disabled: bool,
	/// Coupons never apply to this product. Sales still do.
	pub coupons_disabled: bool,
	/// Cosmetics the recipient must own first.
	pub requires: Vec<i32>,
	/// Whether every cosmetic in `requires` is needed, or any one.
	pub requires_all: bool,
	/// A rental: owned for this many days from purchase.
	pub expires_after_days: Option<i32>,
}

pub(crate) async fn infos(
	db: &impl ConnectionTrait,
	keys: &[Key],
) -> Result<HashMap<Key, SettingsInfo>, DbErr> {
	let settings = load(db, keys).await?;
	let sold = sold(db, &settings).await?;

	Ok(settings
		.into_iter()
		.map(|(key, settings)| {
			let model = settings.model;
			let info = SettingsInfo {
				for_sale: model.for_sale,
				available_from: model.available_from,
				available_until: model.available_until,
				stock_limit: model.stock_limit,
				stock_remaining: model.stock_limit.map(|limit| {
					(i64::from(limit) - sold.get(&key).copied().unwrap_or(0)).max(0)
				}),
				customer_limit: model.customer_limit,
				customer_limit_days: model.customer_limit_days,
				gifting_disabled: model.gifting_disabled,
				coupons_disabled: model.coupons_disabled,
				requires: settings.requirements,
				requires_all: model.requires_all,
				expires_after_days: model.expires_after_days,
			};
			(key, info)
		})
		.collect())
}

/// Mirrors what PayNow can enforce itself, so a checkout opened outside the
/// backend is held to the same rules. Stock and requirements stay here:
/// PayNow counts and checks the payer, not the player receiving the gift.
pub(crate) async fn push(
	client: &PayNowClient,
	product_id: &str,
	settings: &product_settings::Model,
) -> Result<(), PayNowError> {
	// PayNow disables a product by ending its window now.
	let (enabled_at, enabled_until) = if settings.for_sale {
		(settings.available_from, settings.available_until)
	} else {
		(None, Some(Utc::now().fixed_offset()))
	};
	client
		.update_product(
			product_id,
			&UpsertProduct {
				enabled_at: Some(enabled_at.map(|at| at.to_rfc3339())),
				enabled_until: Some(enabled_until.map(|at| at.to_rfc3339())),
				is_gifting_disabled: Some(settings.gifting_disabled),
				is_coupons_disabled: Some(settings.coupons_disabled),
				..Default::default()
			},
		)
		.await
}

#[cfg(test)]
mod tests {
	use super::*;

	fn settings(requires_all: bool, requirements: Vec<i32>) -> Settings {
		Settings {
			model: product_settings::Model {
				id: 1,
				cosmetic_id: Some(1),
				cosmetic_group_id: None,
				bundle_id: None,
				available_from: None,
				available_until: None,
				stock_limit: None,
				customer_limit: None,
				customer_limit_days: None,
				gifting_disabled: false,
				coupons_disabled: false,
				for_sale: true,
				requires_all,
				expires_after_days: None,
			},
			requirements,
		}
	}

	#[test]
	fn requirements_are_all_or_any() {
		let owned = HashSet::from([1]);
		assert!(!settings(true, vec![1, 2]).requirements_met(&owned));
		assert!(settings(false, vec![1, 2]).requirements_met(&owned));
		assert!(settings(false, vec![]).requirements_met(&owned));
	}

	#[test]
	fn the_window_includes_its_start_and_excludes_its_end() {
		let start: DateTime<FixedOffset> = "2026-01-01T00:00:00Z".parse().unwrap();
		let end: DateTime<FixedOffset> = "2026-02-01T00:00:00Z".parse().unwrap();
		let mut timed = settings(true, vec![]);
		timed.model.available_from = Some(start);
		timed.model.available_until = Some(end);

		assert!(timed.is_available(start));
		assert!(!timed.is_available(end));
		assert!(!timed.is_available(start - Duration::seconds(1)));

		timed.model.for_sale = false;
		assert!(!timed.is_available(start));
	}
}
