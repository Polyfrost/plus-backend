use std::collections::HashMap;

use chrono::{Days, NaiveDate, Utc};
use entities::{analytics_cohort_retention, daily_playtime, prelude::*, user};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	QueryFilter as _, QuerySelect as _,
	prelude::DateTimeWithTimeZone,
	sea_query::{Expr, Func},
};

use super::{CountValue, count_in_day, day_bounds};

const RETENTION_OFFSETS: [i32; 16] =
	[0, 1, 2, 3, 4, 5, 6, 7, 14, 21, 28, 30, 60, 90, 180, 365];

pub(super) async fn collect_retention_cells(
	txn: &DatabaseTransaction,
	from: NaiveDate,
	to: NaiveDate,
) -> Result<Vec<analytics_cohort_retention::ActiveModel>, DbErr> {
	let mut sizes: HashMap<NaiveDate, i32> = HashMap::new();
	let mut cells = Vec::new();
	let computed_at = DateTimeWithTimeZone::from(Utc::now());

	let mut activity_day = from;
	loop {
		for offset in RETENTION_OFFSETS {
			let Some(cohort_day) =
				activity_day.checked_sub_days(Days::new(offset.unsigned_abs().into()))
			else {
				continue;
			};

			let size = match sizes.get(&cohort_day) {
				Some(&size) => size,
				None => {
					let size = count_in_day(
						txn,
						User::find(),
						user::Column::CreatedAt,
						day_bounds(cohort_day),
					)
					.await?;
					sizes.insert(cohort_day, size);
					size
				}
			};

			if size == 0 {
				continue;
			}

			cells.push(analytics_cohort_retention::ActiveModel {
				cohort_day: ActiveValue::Set(cohort_day),
				day_offset: ActiveValue::Set(offset),
				cohort_size: ActiveValue::Set(size),
				retained: ActiveValue::Set(
					retained_players(txn, cohort_day, activity_day).await?,
				),
				computed_at: ActiveValue::Set(computed_at),
			});
		}

		let Some(next) = activity_day.succ_opt() else {
			break;
		};
		if next > to {
			break;
		}
		activity_day = next;
	}

	Ok(cells)
}

async fn retained_players(
	txn: &DatabaseTransaction,
	cohort_day: NaiveDate,
	activity_day: NaiveDate,
) -> Result<i32, DbErr> {
	let (cohort_start, cohort_end) = day_bounds(cohort_day);

	let value = DailyPlaytime::find()
		.inner_join(User)
		.filter(daily_playtime::Column::Day.eq(activity_day))
		.filter(daily_playtime::Column::TotalSeconds.gt(0))
		.filter(user::Column::CreatedAt.gte(cohort_start))
		.filter(user::Column::CreatedAt.lt(cohort_end))
		.select_only()
		.column_as(
			Expr::expr(Func::count_distinct(Expr::col((
				daily_playtime::Entity,
				daily_playtime::Column::PlayerId,
			)))),
			"value",
		)
		.into_model::<CountValue>()
		.one(txn)
		.await?
		.unwrap_or_default();

	Ok(value
		.value
		.unwrap_or_default()
		.try_into()
		.unwrap_or(i32::MAX))
}
