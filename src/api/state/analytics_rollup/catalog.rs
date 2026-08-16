use chrono::{Datelike as _, Days, NaiveDate};
use entities::{
	analytics_cosmetic_daily, analytics_cosmetic_snapshot, analytics_slot_snapshot,
	player_equipped_cosmetic, player_owned_cosmetic, prelude::*,
	sea_orm_active_enums::TransactionProvider,
};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	FromQueryResult, QueryFilter as _, QuerySelect as _,
	prelude::DateTimeWithTimeZone,
	sea_query::{Expr, Func, SimpleExpr},
};

use super::day_bounds;

#[derive(Debug, FromQueryResult)]
struct CosmeticAcquisitions {
	cosmetic_id: i32,
	acquisitions: i64,
	paid: i64,
	granted: i64,
}

fn count_where(condition: SimpleExpr) -> SimpleExpr {
	Func::coalesce([
		Expr::expr(Expr::case(condition, 1).finally(0)).sum(),
		Expr::val(0i64).into(),
	])
	.into()
}

pub(super) async fn cosmetic_rows(
	txn: &DatabaseTransaction,
	day: NaiveDate,
	computed_at: DateTimeWithTimeZone,
) -> Result<Vec<analytics_cosmetic_daily::ActiveModel>, DbErr> {
	let (start, end) = day_bounds(day);

	let rows = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::AcquiredAt.gte(start))
		.filter(player_owned_cosmetic::Column::AcquiredAt.lt(end))
		.select_only()
		.column(player_owned_cosmetic::Column::CosmeticId)
		.column_as(
			player_owned_cosmetic::Column::CosmeticId.count(),
			"acquisitions",
		)
		.column_as(
			count_where(
				player_owned_cosmetic::Column::AcquiredVia
					.eq(TransactionProvider::Stripe),
			),
			"paid",
		)
		.column_as(
			count_where(
				player_owned_cosmetic::Column::AcquiredVia
					.eq(TransactionProvider::AdminGrant),
			),
			"granted",
		)
		.group_by(player_owned_cosmetic::Column::CosmeticId)
		.into_model::<CosmeticAcquisitions>()
		.all(txn)
		.await?;

	Ok(rows
		.into_iter()
		.map(|row| analytics_cosmetic_daily::ActiveModel {
			day: ActiveValue::Set(day),
			cosmetic_id: ActiveValue::Set(row.cosmetic_id),
			acquisitions: ActiveValue::Set(
				row.acquisitions.try_into().unwrap_or(i32::MAX),
			),
			acquisitions_paid: ActiveValue::Set(row.paid.try_into().unwrap_or(i32::MAX)),
			acquisitions_granted: ActiveValue::Set(
				row.granted.try_into().unwrap_or(i32::MAX),
			),
			views: ActiveValue::NotSet,
			computed_at: ActiveValue::Set(computed_at),
		})
		.collect())
}

#[derive(Debug, FromQueryResult)]
struct CosmeticCount {
	cosmetic_id: i32,
	total: i64,
}

#[derive(Debug, FromQueryResult)]
struct SlotCount {
	slot: entities::sea_orm_active_enums::BodySlot,
	total: i64,
}

pub(super) fn week_start(day: NaiveDate) -> NaiveDate {
	let back = u64::from(day.weekday().num_days_from_monday());
	day.checked_sub_days(Days::new(back)).unwrap_or(day)
}

/// Past weeks cannot be reconstructed, so only the current week is written.
pub(super) async fn snapshot_rows(
	txn: &DatabaseTransaction,
	week: NaiveDate,
	computed_at: DateTimeWithTimeZone,
) -> Result<
	(
		Vec<analytics_cosmetic_snapshot::ActiveModel>,
		Vec<analytics_slot_snapshot::ActiveModel>,
	),
	DbErr,
> {
	let owners = PlayerOwnedCosmetic::find()
		.select_only()
		.column(player_owned_cosmetic::Column::CosmeticId)
		.column_as(player_owned_cosmetic::Column::PlayerId.count(), "total")
		.group_by(player_owned_cosmetic::Column::CosmeticId)
		.into_model::<CosmeticCount>()
		.all(txn)
		.await?;

	let equipped = PlayerEquippedCosmetic::find()
		.select_only()
		.column(player_equipped_cosmetic::Column::CosmeticId)
		.column_as(player_equipped_cosmetic::Column::PlayerId.count(), "total")
		.group_by(player_equipped_cosmetic::Column::CosmeticId)
		.into_model::<CosmeticCount>()
		.all(txn)
		.await?;

	let equipped_by_cosmetic: std::collections::HashMap<i32, i64> = equipped
		.into_iter()
		.map(|row| (row.cosmetic_id, row.total))
		.collect();

	let cosmetics = owners
		.into_iter()
		.map(|row| analytics_cosmetic_snapshot::ActiveModel {
			week_start: ActiveValue::Set(week),
			cosmetic_id: ActiveValue::Set(row.cosmetic_id),
			owners: ActiveValue::Set(row.total.try_into().unwrap_or(i32::MAX)),
			equipped: ActiveValue::Set(
				equipped_by_cosmetic
					.get(&row.cosmetic_id)
					.copied()
					.unwrap_or_default()
					.try_into()
					.unwrap_or(i32::MAX),
			),
			computed_at: ActiveValue::Set(computed_at),
		})
		.collect();

	let slots = PlayerEquippedCosmetic::find()
		.select_only()
		.column(player_equipped_cosmetic::Column::Slot)
		.column_as(player_equipped_cosmetic::Column::PlayerId.count(), "total")
		.group_by(player_equipped_cosmetic::Column::Slot)
		.into_model::<SlotCount>()
		.all(txn)
		.await?
		.into_iter()
		.map(|row| analytics_slot_snapshot::ActiveModel {
			week_start: ActiveValue::Set(week),
			slot: ActiveValue::Set(row.slot),
			equipped_players: ActiveValue::Set(row.total.try_into().unwrap_or(i32::MAX)),
			computed_at: ActiveValue::Set(computed_at),
		})
		.collect();

	Ok((cosmetics, slots))
}
