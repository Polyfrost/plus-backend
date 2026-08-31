use chrono::{Datelike as _, Days, NaiveDate};
use entities::{
	analytics_cosmetic_daily, analytics_cosmetic_snapshot, analytics_slot_snapshot,
	cosmetic_ownership_event, player_equipped_cosmetic, player_owned_cosmetic,
	prelude::*,
	sea_orm_active_enums::{OwnershipEventKind, TransactionProvider, TransactionStatus},
	transaction, transaction_line,
};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	FromQueryResult, QueryFilter as _, QuerySelect as _,
	prelude::DateTimeWithTimeZone,
	sea_query::{Expr, Func, SimpleExpr},
};

use super::day_bounds;

const PURCHASE_PROVIDERS: [TransactionProvider; 2] =
	[TransactionProvider::Stripe, TransactionProvider::Paynow];

#[derive(Debug, FromQueryResult)]
struct CosmeticAcquisitions {
	cosmetic_id: i32,
	acquisitions: i64,
	purchased: i64,
	paid: i64,
	granted: i64,
}

#[derive(Debug, FromQueryResult)]
struct LineAcquisition {
	cosmetic_id: i32,
	transaction_line_id: i64,
	total_minor: i64,
}

#[derive(Debug, FromQueryResult)]
struct LineReturn {
	cosmetic_id: i32,
	player_id: i32,
	transaction_line_id: i64,
	returned_minor: i64,
	status: TransactionStatus,
}

/// What came back on a day, per cosmetic, kept apart by how it came back: a
/// refund is a customer changing their mind, a chargeback is a dispute.
#[derive(Debug, Default, Clone, Copy)]
struct Returns {
	refunded: i64,
	charged_back: i64,
	refunded_minor: i64,
	charged_back_minor: i64,
}

/// The acquisition's own line, falling back to the order total for rows
/// written before lines existed.
fn amount_paid() -> SimpleExpr {
	Func::coalesce([
		Expr::col((
			transaction_line::Entity,
			transaction_line::Column::TotalMinor,
		))
		.into(),
		Expr::col((transaction::Entity, transaction::Column::AmountMinor)).into(),
		Expr::value(0i64),
	])
	.into()
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
		.left_join(Transaction)
		.left_join(TransactionLine)
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
				player_owned_cosmetic::Column::AcquiredVia.is_in(PURCHASE_PROVIDERS),
			),
			"purchased",
		)
		// Priced from the line that delivered it, so a free item in a paid
		// basket is no longer counted as paid.
		.column_as(
			count_where(
				player_owned_cosmetic::Column::AcquiredVia
					.is_in(PURCHASE_PROVIDERS)
					.and(Expr::expr(amount_paid()).gt(0)),
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

	let revenue = revenue_by_cosmetic(txn, start, end).await?;
	let returns = returns_by_cosmetic(txn, start, end).await?;

	// A cosmetic returned today was usually bought on some earlier day, so it
	// has no acquisition row to hang the return off. Give it one.
	let acquired: std::collections::HashSet<i32> =
		rows.iter().map(|row| row.cosmetic_id).collect();
	let rows = rows.into_iter().chain(
		returns
			.keys()
			.filter(|cosmetic_id| !acquired.contains(cosmetic_id))
			.map(|cosmetic_id| CosmeticAcquisitions {
				cosmetic_id: *cosmetic_id,
				acquisitions: 0,
				purchased: 0,
				paid: 0,
				granted: 0,
			})
			.collect::<Vec<_>>(),
	);

	Ok(rows
		.map(|row| {
			let returned = returns.get(&row.cosmetic_id).copied().unwrap_or_default();
			analytics_cosmetic_daily::ActiveModel {
				day: ActiveValue::Set(day),
				cosmetic_id: ActiveValue::Set(row.cosmetic_id),
				acquisitions: ActiveValue::Set(
					row.acquisitions.try_into().unwrap_or(i32::MAX),
				),
				acquisitions_paid: ActiveValue::Set(
					row.paid.try_into().unwrap_or(i32::MAX),
				),
				acquisitions_free: ActiveValue::Set(
					(row.purchased - row.paid).try_into().unwrap_or(i32::MAX),
				),
				acquisitions_granted: ActiveValue::Set(
					row.granted.try_into().unwrap_or(i32::MAX),
				),
				revenue_minor: ActiveValue::Set(
					revenue.get(&row.cosmetic_id).copied().unwrap_or_default(),
				),
				refunded: ActiveValue::Set(
					returned.refunded.try_into().unwrap_or(i32::MAX),
				),
				charged_back: ActiveValue::Set(
					returned.charged_back.try_into().unwrap_or(i32::MAX),
				),
				refunded_minor: ActiveValue::Set(returned.refunded_minor),
				charged_back_minor: ActiveValue::Set(returned.charged_back_minor),
				views: ActiveValue::NotSet,
				computed_at: ActiveValue::Set(computed_at),
			}
		})
		.collect())
}

async fn returns_by_cosmetic(
	txn: &DatabaseTransaction,
	start: DateTimeWithTimeZone,
	end: DateTimeWithTimeZone,
) -> Result<std::collections::HashMap<i32, Returns>, DbErr> {
	let events = CosmeticOwnershipEvent::find()
		.inner_join(TransactionLine)
		.filter(cosmetic_ownership_event::Column::Kind.eq(OwnershipEventKind::Revoked))
		.filter(transaction_line::Column::ReturnedAt.gte(start))
		.filter(transaction_line::Column::ReturnedAt.lt(end))
		.select_only()
		.column(cosmetic_ownership_event::Column::CosmeticId)
		.column(cosmetic_ownership_event::Column::PlayerId)
		.column(cosmetic_ownership_event::Column::TransactionLineId)
		.column(transaction_line::Column::ReturnedMinor)
		.column(transaction_line::Column::Status)
		.into_model::<LineReturn>()
		.all(txn)
		.await?;

	let mut seen = std::collections::HashSet::new();
	let events: Vec<LineReturn> = events
		.into_iter()
		.filter(|event| {
			seen.insert((
				event.player_id,
				event.cosmetic_id,
				event.transaction_line_id,
			))
		})
		.collect();

	let mut per_line: std::collections::HashMap<i64, i64> =
		std::collections::HashMap::new();
	for event in &events {
		*per_line.entry(event.transaction_line_id).or_default() += 1;
	}

	let mut returns: std::collections::HashMap<i32, Returns> =
		std::collections::HashMap::new();
	for event in events {
		let share = per_line
			.get(&event.transaction_line_id)
			.copied()
			.unwrap_or(1)
			.max(1);
		let amount = event.returned_minor / share;
		let entry = returns.entry(event.cosmetic_id).or_default();

		if matches!(event.status, TransactionStatus::Chargeback) {
			entry.charged_back += 1;
			entry.charged_back_minor += amount;
		} else {
			entry.refunded += 1;
			entry.refunded_minor += amount;
		}
	}

	Ok(returns)
}

async fn revenue_by_cosmetic(
	txn: &DatabaseTransaction,
	start: DateTimeWithTimeZone,
	end: DateTimeWithTimeZone,
) -> Result<std::collections::HashMap<i32, i64>, DbErr> {
	let acquisitions = PlayerOwnedCosmetic::find()
		.inner_join(TransactionLine)
		.filter(player_owned_cosmetic::Column::AcquiredAt.gte(start))
		.filter(player_owned_cosmetic::Column::AcquiredAt.lt(end))
		.select_only()
		.column(player_owned_cosmetic::Column::CosmeticId)
		.column(player_owned_cosmetic::Column::TransactionLineId)
		.column(transaction_line::Column::TotalMinor)
		.into_model::<LineAcquisition>()
		.all(txn)
		.await?;

	let mut per_line: std::collections::HashMap<i64, i64> =
		std::collections::HashMap::new();
	for acquisition in &acquisitions {
		*per_line.entry(acquisition.transaction_line_id).or_default() += 1;
	}

	let mut revenue: std::collections::HashMap<i32, i64> =
		std::collections::HashMap::new();
	for acquisition in acquisitions {
		let share = per_line
			.get(&acquisition.transaction_line_id)
			.copied()
			.unwrap_or(1)
			.max(1);
		*revenue.entry(acquisition.cosmetic_id).or_default() +=
			acquisition.total_minor / share;
	}

	Ok(revenue)
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
