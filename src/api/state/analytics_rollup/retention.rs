use std::collections::HashMap;

use chrono::{Days, NaiveDate, Utc};
use entities::{
	analytics_cohort_retention, daily_playtime, monthly_active_login, prelude::*, user,
};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	FromQueryResult, QueryFilter as _, QuerySelect as _, prelude::DateTimeWithTimeZone,
	sea_query::Query,
};

use super::day_bounds;
use crate::utils::time::start_of_day;

const RETENTION_OFFSETS: [i32; 16] =
	[0, 1, 2, 3, 4, 5, 6, 7, 14, 21, 28, 30, 60, 90, 180, 365];
const MAX_OFFSET: u64 = 365;

#[derive(Debug, FromQueryResult)]
struct CohortMember {
	id: i32,
	created_at: DateTimeWithTimeZone,
}

#[derive(Debug, FromQueryResult)]
struct ActiveDay {
	player_id: i32,
	day: NaiveDate,
}

/// Cohorts only hold accounts that have logged in at least once; lookups and
/// friend requests create users who never will. `from` of `None` rebuilds
/// every cohort.
pub(super) async fn collect_retention_cells(
	txn: &DatabaseTransaction,
	from: Option<NaiveDate>,
	to: NaiveDate,
) -> Result<Vec<analytics_cohort_retention::ActiveModel>, DbErr> {
	let (_, end) = day_bounds(to);
	let mut members = User::find()
		.filter(user::Column::CreatedAt.lt(end))
		.filter(
			user::Column::Id.in_subquery(
				Query::select()
					.column(monthly_active_login::Column::PlayerId)
					.from(MonthlyActiveLogin)
					.to_owned(),
			),
		)
		.select_only()
		.column(user::Column::Id)
		.column(user::Column::CreatedAt);
	if let Some(from) = from {
		let floor = from.checked_sub_days(Days::new(MAX_OFFSET)).unwrap_or(from);
		members = members.filter(user::Column::CreatedAt.gte(start_of_day(floor)));
	}

	let cohorts: HashMap<i32, NaiveDate> = members
		.into_model::<CohortMember>()
		.all(txn)
		.await?
		.into_iter()
		.map(|member| (member.id, member.created_at.naive_utc().date()))
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
