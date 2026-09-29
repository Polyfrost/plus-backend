use std::collections::{HashMap, HashSet};

use entities::{discount, discount_redemption, discount_target, prelude::*};
use sea_orm::{
	ActiveValue, ColumnTrait as _, Condition, DbErr, EntityTrait, FromQueryResult,
	QueryFilter as _, QuerySelect as _, Set, prelude::*, sea_query::Expr,
};

use super::{Rule, Targets, amount_of};

/// A rule alongside the row it came from, for callers that need both — the
/// admin dashboard wants the window and the counters, checkout wants the rule.
#[derive(Debug, Clone)]
pub(crate) struct LiveDiscount {
	pub model: discount::Model,
	pub rule: Rule,
}

/// Codes are stored and compared upper case, so the buyer can type whatever
/// they like.
pub(crate) fn normalise_code(code: &str) -> String {
	code.trim().to_uppercase()
}

#[derive(Debug, FromQueryResult)]
struct RedemptionCount {
	discount_id: i32,
	total: i64,
}

/// Every discount in force right now: the automatic sales, plus any coupon
/// whose code was supplied and whose limits still leave room.
///
/// Pass no codes and no player to price the storefront, which is what the
/// client mod reads.
pub(crate) async fn live_rules(
	db: &impl ConnectionTrait,
	codes: &[String],
	player_id: Option<i32>,
	now: DateTimeWithTimeZone,
) -> Result<Vec<LiveDiscount>, DbErr> {
	let codes: Vec<String> = codes.iter().map(|code| normalise_code(code)).collect();

	let mut wanted = Condition::any().add(discount::Column::Code.is_null());
	if !codes.is_empty() {
		wanted = wanted.add(discount::Column::Code.is_in(codes));
	}

	let rows = Discount::find()
		.filter(wanted)
		.filter(discount::Column::Enabled.eq(true))
		.filter(
			Condition::any()
				.add(discount::Column::StartsAt.is_null())
				.add(discount::Column::StartsAt.lte(now)),
		)
		.filter(
			Condition::any()
				.add(discount::Column::EndsAt.is_null())
				.add(discount::Column::EndsAt.gt(now)),
		)
		.filter(
			Condition::any()
				.add(discount::Column::MaxRedemptions.is_null())
				.add(
					Expr::col(discount::Column::Redemptions)
						.lt(Expr::col(discount::Column::MaxRedemptions)),
				),
		)
		.all(db)
		.await?;
	if rows.is_empty() {
		return Ok(Vec::new());
	}

	let exhausted = per_player_exhausted(db, &rows, player_id).await?;
	let rows: Vec<discount::Model> = rows
		.into_iter()
		.filter(|row| !exhausted.contains(&row.id))
		.collect();

	let mut targets = targets_by_discount(db, &rows).await?;

	Ok(rows
		.into_iter()
		.filter_map(|model| {
			// A row the check constraint would have rejected; skip it rather
			// than price something nobody can explain.
			let amount = amount_of(&model)?;
			let rule = Rule {
				id: model.id,
				name: model.name.clone(),
				code: model.code.clone(),
				amount,
				applies_to_all: model.applies_to_all,
				targets: targets.remove(&model.id).unwrap_or_default(),
				min_subtotal_minor: model.min_subtotal_minor,
			};

			Some(LiveDiscount { model, rule })
		})
		.collect())
}

/// Which of these the player has already used up. Only coded rows carry a
/// per-player limit, so an anonymous quote never pays for this query.
async fn per_player_exhausted(
	db: &impl ConnectionTrait,
	rows: &[discount::Model],
	player_id: Option<i32>,
) -> Result<HashSet<i32>, DbErr> {
	let limited: HashMap<i32, i32> = rows
		.iter()
		.filter_map(|row| row.max_per_player.map(|limit| (row.id, limit)))
		.collect();

	let (Some(player_id), false) = (player_id, limited.is_empty()) else {
		return Ok(HashSet::new());
	};

	let counts = DiscountRedemption::find()
		.filter(discount_redemption::Column::PlayerId.eq(player_id))
		.filter(
			discount_redemption::Column::DiscountId
				.is_in(limited.keys().copied().collect::<Vec<_>>()),
		)
		.select_only()
		.column(discount_redemption::Column::DiscountId)
		.column_as(discount_redemption::Column::Id.count(), "total")
		.group_by(discount_redemption::Column::DiscountId)
		.into_model::<RedemptionCount>()
		.all(db)
		.await?;

	Ok(counts
		.into_iter()
		.filter(|count| {
			limited
				.get(&count.discount_id)
				.is_some_and(|limit| count.total >= i64::from(*limit))
		})
		.map(|count| count.discount_id)
		.collect())
}

pub(crate) async fn targets_by_discount(
	db: &impl ConnectionTrait,
	rows: &[discount::Model],
) -> Result<HashMap<i32, Targets>, DbErr> {
	let scoped: Vec<i32> = rows
		.iter()
		.filter(|row| !row.applies_to_all)
		.map(|row| row.id)
		.collect();
	if scoped.is_empty() {
		return Ok(HashMap::new());
	}

	let mut targets: HashMap<i32, Targets> = HashMap::new();
	for row in DiscountTarget::find()
		.filter(discount_target::Column::DiscountId.is_in(scoped))
		.all(db)
		.await?
	{
		let entry = targets.entry(row.discount_id).or_default();
		if let Some(id) = row.collection_id {
			entry.collections.push(id);
		}
		if let Some(id) = row.tag_id {
			entry.tags.push(id);
		}
		if let Some(id) = row.cosmetic_id {
			entry.cosmetics.push(id);
		}
		if let Some(id) = row.cosmetic_group_id {
			entry.cosmetic_groups.push(id);
		}
		if let Some(id) = row.bundle_id {
			entry.bundles.push(id);
		}
	}

	Ok(targets)
}

/// The discounts mirrored by these PayNow sale and coupon ids.
pub(crate) async fn by_paynow_ids(
	db: &impl ConnectionTrait,
	paynow_ids: &[String],
) -> Result<Vec<i32>, DbErr> {
	if paynow_ids.is_empty() {
		return Ok(Vec::new());
	}

	Ok(Discount::find()
		.filter(discount::Column::PaynowId.is_in(paynow_ids.iter().cloned()))
		.all(db)
		.await?
		.into_iter()
		.map(|row| row.id)
		.collect())
}

/// Records that a discount was used, so per-player and total limits hold and
/// the dashboard can say which code sold what.
pub(crate) async fn redeem(
	db: &impl ConnectionTrait,
	discount_id: i32,
	player_id: i32,
	transaction_id: Option<i32>,
) -> Result<(), DbErr> {
	DiscountRedemption::insert(discount_redemption::ActiveModel {
		discount_id: Set(discount_id),
		player_id: Set(player_id),
		transaction_id: Set(transaction_id),
		redeemed_at: ActiveValue::NotSet,
		id: ActiveValue::NotSet,
	})
	.exec_without_returning(db)
	.await?;

	Discount::update_many()
		.col_expr(
			discount::Column::Redemptions,
			Expr::col(discount::Column::Redemptions).add(1),
		)
		.filter(discount::Column::Id.eq(discount_id))
		.exec(db)
		.await?;

	Ok(())
}
