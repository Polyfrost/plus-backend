use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{NaiveDate, Utc};
use entities::{analytics_cohort_retention, prelude::*};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait as _, EntityTrait, QueryFilter as _, QueryOrder as _};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, resolve_series_bounds,
};
use crate::api::ApiState;

pub(super) fn retention_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsRetention")
		.summary("Get signup cohort retention")
		.description(
			"Returns, for each signup day in the period, how many of that day's new \
			 accounts played again a fixed number of days later.\n\nOffsets are fixed \
			 (0-7, 14, 21, 28, 30, 60, 90, 180, 365). An offset is absent until it has \
			 actually elapsed for that cohort, so a missing entry means \"not knowable \
			 yet\" rather than zero retention. `start` and `end` bound the cohort days \
			 and default the same way as `/analytics/daily`.",
		)
		.tag("analytics")
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct RetentionResponse {
	start: NaiveDate,
	end: NaiveDate,
	cohorts: Vec<RetentionCohort>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct RetentionCohort {
	cohort_day: NaiveDate,
	cohort_size: i32,
	offsets: Vec<RetentionPoint>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct RetentionPoint {
	day_offset: i32,
	retained: i32,
	/// `retained / cohort_size`, in 0.0..=1.0.
	rate: f64,
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn retention_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<RetentionResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;

	let rows = AnalyticsCohortRetention::find()
		.filter(analytics_cohort_retention::Column::CohortDay.gte(start))
		.filter(analytics_cohort_retention::Column::CohortDay.lte(end))
		.order_by_asc(analytics_cohort_retention::Column::CohortDay)
		.order_by_asc(analytics_cohort_retention::Column::DayOffset)
		.all(&state.database)
		.await?;

	// Rows arrive ordered by cohort then offset, so cohorts are contiguous.
	let mut cohorts: Vec<RetentionCohort> = Vec::new();
	for row in rows {
		let point = RetentionPoint {
			day_offset: row.day_offset,
			retained: row.retained,
			rate: if row.cohort_size > 0 {
				f64::from(row.retained) / f64::from(row.cohort_size)
			} else {
				0.0
			},
		};

		match cohorts.last_mut() {
			Some(cohort) if cohort.cohort_day == row.cohort_day => {
				cohort.offsets.push(point);
			}
			_ => cohorts.push(RetentionCohort {
				cohort_day: row.cohort_day,
				cohort_size: row.cohort_size,
				offsets: vec![point],
			}),
		}
	}

	Ok(Json(RetentionResponse {
		start,
		end,
		cohorts,
	}))
}
