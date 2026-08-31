mod catalog;
mod clients;
mod daily;
mod hourly;
mod retention;

pub(in crate::api) use self::hourly::SESSION_LENGTH_BUCKETS;

use std::time::{Duration, Instant};

use chrono::{Days, NaiveDate, Utc};
use entities::{
	analytics_client_daily, analytics_cohort_retention, analytics_cosmetic_daily,
	analytics_cosmetic_snapshot, analytics_daily, analytics_hourly, analytics_job_state,
	analytics_session_length_daily, analytics_slot_snapshot, prelude::*, user,
};
use sea_orm::{
	ActiveValue, ColumnTrait as _, ConnectionTrait as _, DatabaseConnection,
	DatabaseTransaction, DbErr, EntityTrait, FromQueryResult, PaginatorTrait as _,
	QueryFilter as _, QuerySelect as _, Select, TransactionTrait as _,
	prelude::DateTimeWithTimeZone,
	sea_query::{Expr, OnConflict},
};
use tracing::{info, warn};

use crate::utils::time::start_of_day;

const ROLLUP_INTERVAL: Duration = Duration::from_secs(15 * 60);
const BACKFILL_INTERVAL: Duration = Duration::from_secs(30);
const LATE_ARRIVAL_DAYS: u64 = 3;
const MAX_DAYS_PER_RUN: u64 = 14;
const STATEMENT_TIMEOUT: &str = "120s";
pub(crate) const ANALYTICS_DAILY_JOB: &str = "analytics_daily";

#[derive(Debug, Default, FromQueryResult)]
pub(super) struct CountValue {
	value: Option<i64>,
}

#[derive(Debug, FromQueryResult)]
struct EarliestUser {
	earliest: Option<DateTimeWithTimeZone>,
}

pub(super) fn spawn_analytics_rollup(database: DatabaseConnection) {
	tokio::spawn(rollup_loop(database));
}

enum Rollup {
	CaughtUp,
	Backfilling,
}

async fn rollup_loop(database: DatabaseConnection) {
	loop {
		let delay = match run_rollup(&database).await {
			Ok(Rollup::CaughtUp) => ROLLUP_INTERVAL,
			Ok(Rollup::Backfilling) => BACKFILL_INTERVAL,
			Err(error) => {
				warn!("Unable to roll up analytics: {error}");
				if let Err(error) = record_failure(&database, &error.to_string()).await {
					warn!("Unable to record analytics rollup failure: {error}");
				}
				ROLLUP_INTERVAL
			}
		};

		tokio::time::sleep(delay).await;
	}
}

async fn run_rollup(database: &DatabaseConnection) -> Result<Rollup, DbErr> {
	let finalized_through = load_watermark(database).await?;
	let today = Utc::now().date_naive();

	let Some((from, to, finalized)) = recompute_window(finalized_through, today) else {
		return Ok(Rollup::CaughtUp);
	};

	let started = Instant::now();

	let txn = database.begin().await?;

	// SET LOCAL reverts on commit; a plain SET would leak the timeout onto
	// the pooled connection.
	txn.execute_unprepared(&format!(
		"SET LOCAL statement_timeout = '{STATEMENT_TIMEOUT}'; \
		 SET LOCAL lock_timeout = '5s'"
	))
	.await?;

	let now = Utc::now();
	let computed_at = DateTimeWithTimeZone::from(now);
	let mut rows = Vec::new();
	let mut hourly = Vec::new();
	let mut lengths = Vec::new();
	let mut cosmetics = Vec::new();
	let mut clients = Vec::new();
	let mut day = from;
	while day <= to {
		rows.push(daily::collect_day(&txn, day).await?);

		let spans = hourly::session_spans_for_day(&txn, day, now).await?;
		hourly.extend(hourly::hourly_rows(
			day,
			hourly::bucket_sessions_into_hours(day, &spans),
			computed_at,
		));
		lengths.extend(hourly::session_length_rows(day, &spans, computed_at));
		cosmetics.extend(catalog::cosmetic_rows(&txn, day, computed_at).await?);
		clients.extend(clients::client_rows(&txn, day, computed_at).await?);

		let Some(next) = day.succ_opt() else { break };
		day = next;
	}

	if !hourly.is_empty() {
		AnalyticsHourly::insert_many(hourly)
			.on_conflict(
				OnConflict::column(analytics_hourly::Column::HourStart)
					.update_columns([
						analytics_hourly::Column::Dow,
						analytics_hourly::Column::HourOfDay,
						analytics_hourly::Column::PlaytimeSeconds,
						analytics_hourly::Column::ActivePlayers,
						analytics_hourly::Column::SessionsStarted,
						analytics_hourly::Column::PeakConcurrent,
						analytics_hourly::Column::ComputedAt,
					])
					.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	if !clients.is_empty() {
		AnalyticsClientDaily::insert_many(clients)
			.on_conflict(
				OnConflict::columns([
					analytics_client_daily::Column::Day,
					analytics_client_daily::Column::ClientVersion,
					analytics_client_daily::Column::MinecraftVersion,
					analytics_client_daily::Column::Loader,
					analytics_client_daily::Column::Os,
				])
				.update_columns([
					analytics_client_daily::Column::ActivePlayers,
					analytics_client_daily::Column::ComputedAt,
				])
				.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	if !cosmetics.is_empty() {
		AnalyticsCosmeticDaily::insert_many(cosmetics)
			.on_conflict(
				OnConflict::columns([
					analytics_cosmetic_daily::Column::Day,
					analytics_cosmetic_daily::Column::CosmeticId,
				])
				.update_columns([
					analytics_cosmetic_daily::Column::Acquisitions,
					analytics_cosmetic_daily::Column::AcquisitionsPaid,
					analytics_cosmetic_daily::Column::AcquisitionsFree,
					analytics_cosmetic_daily::Column::AcquisitionsGranted,
					analytics_cosmetic_daily::Column::RevenueMinor,
					analytics_cosmetic_daily::Column::Refunded,
					analytics_cosmetic_daily::Column::ChargedBack,
					analytics_cosmetic_daily::Column::RefundedMinor,
					analytics_cosmetic_daily::Column::ChargedBackMinor,
					analytics_cosmetic_daily::Column::ComputedAt,
				])
				.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	if !lengths.is_empty() {
		AnalyticsSessionLengthDaily::insert_many(lengths)
			.on_conflict(
				OnConflict::columns([
					analytics_session_length_daily::Column::Day,
					analytics_session_length_daily::Column::Bucket,
				])
				.update_columns([
					analytics_session_length_daily::Column::Sessions,
					analytics_session_length_daily::Column::ComputedAt,
				])
				.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	if !rows.is_empty() {
		AnalyticsDaily::insert_many(rows)
			.on_conflict(
				OnConflict::column(analytics_daily::Column::Day)
					.update_columns(daily::rolled_up_columns())
					.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	let cells = retention::collect_retention_cells(&txn, from, to).await?;
	if !cells.is_empty() {
		AnalyticsCohortRetention::insert_many(cells)
			.on_conflict(
				OnConflict::columns([
					analytics_cohort_retention::Column::CohortDay,
					analytics_cohort_retention::Column::DayOffset,
				])
				.update_columns([
					analytics_cohort_retention::Column::CohortSize,
					analytics_cohort_retention::Column::Retained,
					analytics_cohort_retention::Column::ComputedAt,
				])
				.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	let week = catalog::week_start(today);
	let (cosmetic_snapshot, slot_snapshot) =
		catalog::snapshot_rows(&txn, week, computed_at).await?;

	if !cosmetic_snapshot.is_empty() {
		AnalyticsCosmeticSnapshot::insert_many(cosmetic_snapshot)
			.on_conflict(
				OnConflict::columns([
					analytics_cosmetic_snapshot::Column::WeekStart,
					analytics_cosmetic_snapshot::Column::CosmeticId,
				])
				.update_columns([
					analytics_cosmetic_snapshot::Column::Owners,
					analytics_cosmetic_snapshot::Column::Equipped,
					analytics_cosmetic_snapshot::Column::ComputedAt,
				])
				.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	if !slot_snapshot.is_empty() {
		AnalyticsSlotSnapshot::insert_many(slot_snapshot)
			.on_conflict(
				OnConflict::columns([
					analytics_slot_snapshot::Column::WeekStart,
					analytics_slot_snapshot::Column::Slot,
				])
				.update_columns([
					analytics_slot_snapshot::Column::EquippedPlayers,
					analytics_slot_snapshot::Column::ComputedAt,
				])
				.to_owned(),
			)
			.exec_without_returning(&txn)
			.await?;
	}

	txn.commit().await?;

	let elapsed = started.elapsed().as_millis().try_into().unwrap_or(i32::MAX);
	save_watermark(database, finalized, elapsed).await?;

	if to < today {
		info!(
			%from, %to, %today, elapsed_ms = elapsed,
			"Rolled up analytics, still backfilling"
		);
		return Ok(Rollup::Backfilling);
	}

	info!(%from, %to, elapsed_ms = elapsed, "Rolled up analytics");

	Ok(Rollup::CaughtUp)
}

/// Returns `(from, to, new_finalized_through)`, or `None` when already current.
fn recompute_window(
	finalized_through: NaiveDate,
	today: NaiveDate,
) -> Option<(NaiveDate, NaiveDate, NaiveDate)> {
	let from = finalized_through.checked_add_days(Days::new(1))?;
	if from > today {
		return None;
	}

	let to = from
		.checked_add_days(Days::new(MAX_DAYS_PER_RUN))
		.map_or(today, |capped| capped.min(today));

	let finalized = today
		.checked_sub_days(Days::new(LATE_ARRIVAL_DAYS))
		.map_or(finalized_through, |cutoff| {
			to.min(cutoff).max(finalized_through)
		});

	Some((from, to, finalized))
}

/// Comparing a `timestamptz` against these uses the column's index; casting the
/// column to a date would not.
pub(super) fn day_bounds(day: NaiveDate) -> (DateTimeWithTimeZone, DateTimeWithTimeZone) {
	let start = start_of_day(day);
	let end = day.succ_opt().map_or(start, start_of_day);

	(start, end)
}

pub(super) async fn count_in_day<E>(
	txn: &DatabaseTransaction,
	select: Select<E>,
	column: E::Column,
	(start, end): (DateTimeWithTimeZone, DateTimeWithTimeZone),
) -> Result<i32, DbErr>
where
	E: EntityTrait,
	E::Model: Send + Sync,
{
	let count = select
		.filter(column.gte(start))
		.filter(column.lt(end))
		.count(txn)
		.await?;

	Ok(count.try_into().unwrap_or(i32::MAX))
}

/// Seeds the watermark on first run so the backfill reaches the earliest account.
async fn load_watermark(database: &DatabaseConnection) -> Result<NaiveDate, DbErr> {
	if let Some(state) = AnalyticsJobState::find_by_id(ANALYTICS_DAILY_JOB)
		.one(database)
		.await?
	{
		return Ok(state.finalized_through);
	}

	let today = Utc::now().date_naive();
	let earliest = User::find()
		.select_only()
		.column_as(user::Column::CreatedAt.min(), "earliest")
		.into_model::<EarliestUser>()
		.one(database)
		.await?
		.and_then(|row| row.earliest)
		.map_or(today, |created| created.naive_utc().date());

	let finalized = earliest.checked_sub_days(Days::new(1)).unwrap_or(earliest);

	AnalyticsJobState::insert(analytics_job_state::ActiveModel {
		job_name: ActiveValue::Set(ANALYTICS_DAILY_JOB.to_owned()),
		finalized_through: ActiveValue::Set(finalized),
		..Default::default()
	})
	.on_conflict(
		OnConflict::column(analytics_job_state::Column::JobName)
			.do_nothing()
			.to_owned(),
	)
	.do_nothing()
	.exec(database)
	.await?;

	Ok(finalized)
}

async fn save_watermark(
	database: &DatabaseConnection,
	finalized_through: NaiveDate,
	elapsed_ms: i32,
) -> Result<(), DbErr> {
	AnalyticsJobState::insert(analytics_job_state::ActiveModel {
		job_name: ActiveValue::Set(ANALYTICS_DAILY_JOB.to_owned()),
		finalized_through: ActiveValue::Set(finalized_through),
		last_run_at: ActiveValue::Set(Some(Utc::now().into())),
		last_run_ms: ActiveValue::Set(Some(elapsed_ms)),
		last_error: ActiveValue::Set(None),
	})
	.on_conflict(
		OnConflict::column(analytics_job_state::Column::JobName)
			.update_columns([
				analytics_job_state::Column::FinalizedThrough,
				analytics_job_state::Column::LastRunAt,
				analytics_job_state::Column::LastRunMs,
				analytics_job_state::Column::LastError,
			])
			.to_owned(),
	)
	.exec_without_returning(database)
	.await?;

	Ok(())
}

/// Leaves the watermark alone so the same window is retried next tick.
async fn record_failure(database: &DatabaseConnection, error: &str) -> Result<(), DbErr> {
	AnalyticsJobState::update_many()
		.col_expr(
			analytics_job_state::Column::LastError,
			Expr::value(error.to_owned()),
		)
		.col_expr(
			analytics_job_state::Column::LastRunAt,
			Expr::value(DateTimeWithTimeZone::from(Utc::now())),
		)
		.filter(analytics_job_state::Column::JobName.eq(ANALYTICS_DAILY_JOB))
		.exec(database)
		.await?;

	Ok(())
}
