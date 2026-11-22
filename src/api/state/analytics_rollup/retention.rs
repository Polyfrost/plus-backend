use std::collections::HashMap;

use chrono::{Days, NaiveDate, Utc};
use entities::{analytics_cohort_retention, daily_playtime, prelude::*};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	FromQueryResult, QueryFilter as _, QuerySelect as _, prelude::DateTimeWithTimeZone,
};

use super::day_bounds;
use crate::{
	database::{first_login_at, first_logins},
	utils::time::start_of_day,
};

const RETENTION_OFFSETS: [i32; 16] =
	[0, 1, 2, 3, 4, 5, 6, 7, 14, 21, 28, 30, 60, 90, 180, 365];
const MAX_OFFSET: u64 = 365;

#[derive(Debug, FromQueryResult)]
struct CohortMember {
	player_id: i32,
	first_login_at: DateTimeWithTimeZone,
}

#[derive(Debug, FromQueryResult)]
struct ActiveDay {
	player_id: i32,
	day: NaiveDate,
}

/// Cohorts are players by the day of their first login, the same day
/// `new_users` counts them on. `from` of `None` rebuilds every cohort.
pub(super) async fn collect_retention_cells(
	txn: &DatabaseTransaction,
	from: Option<NaiveDate>,
	to: NaiveDate,
) -> Result<Vec<analytics_cohort_retention::ActiveModel>, DbErr> {
	let (_, end) = day_bounds(to);
	let floor = from.map(|from| {
		start_of_day(from.checked_sub_days(Days::new(MAX_OFFSET)).unwrap_or(from))
	});

	let cohorts: HashMap<i32, NaiveDate> = first_logins(floor, Some(end))
		.column_as(first_login_at(), "first_login_at")
		.into_model::<CohortMember>()
		.all(txn)
		.await?
		.into_iter()
		.map(|member| (member.player_id, member.first_login_at.naive_utc().date()))
		.collect();

	let Some(activity_from) = from.or_else(|| cohorts.values().min().copied()) else {
		return Ok(Vec::new());
	};

	let mut sizes: HashMap<NaiveDate, i32> = HashMap::new();
	for &cohort_day in cohorts.values() {
		*sizes.entry(cohort_day).or_default() += 1;
	}

	let active = DailyPlaytime::find()
		.filter(daily_playtime::Column::Day.gte(activity_from))
		.filter(daily_playtime::Column::Day.lte(to))
		.filter(daily_playtime::Column::TotalSeconds.gt(0))
		.select_only()
		.column(daily_playtime::Column::PlayerId)
		.column(daily_playtime::Column::Day)
		.into_model::<ActiveDay>()
		.all(txn)
		.await?;

	let mut retained: HashMap<(NaiveDate, i32), i32> = HashMap::new();
	for row in active {
		let Some(&cohort_day) = cohorts.get(&row.player_id) else {
			continue;
		};
		let Ok(offset) =
			i32::try_from(row.day.signed_duration_since(cohort_day).num_days())
		else {
			continue;
		};
		if RETENTION_OFFSETS.contains(&offset) {
			*retained.entry((cohort_day, offset)).or_default() += 1;
		}
	}

	let computed_at = DateTimeWithTimeZone::from(Utc::now());
	let mut cells = Vec::new();
	let mut activity_day = activity_from;
	while activity_day <= to {
		for offset in RETENTION_OFFSETS {
			let Some(cohort_day) =
				activity_day.checked_sub_days(Days::new(offset.unsigned_abs().into()))
			else {
				continue;
			};
			let Some(&size) = sizes.get(&cohort_day) else {
				continue;
			};

			cells.push(analytics_cohort_retention::ActiveModel {
				cohort_day: ActiveValue::Set(cohort_day),
				day_offset: ActiveValue::Set(offset),
				cohort_size: ActiveValue::Set(size),
				retained: ActiveValue::Set(
					retained
						.get(&(cohort_day, offset))
						.copied()
						.unwrap_or_default(),
				),
				computed_at: ActiveValue::Set(computed_at),
			});
		}

		let Some(next) = activity_day.succ_opt() else {
			break;
		};
		activity_day = next;
	}

	Ok(cells)
}
