use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use entities::{analytics_hourly, analytics_session_length_daily, prelude::*};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait as _, EntityTrait, QueryFilter as _, QueryOrder as _};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, resolve_series_bounds,
};
use crate::{
	api::{ApiState, state::SESSION_LENGTH_BUCKETS},
	utils::time::start_of_day,
};

pub(super) fn sessions_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsSessions")
		.summary("Get session length and concurrency")
		.description(
			"Session-length distribution and concurrency peaks over the period.\n\n\
			 Lengths are stored bucketed, so the percentiles are interpolated within \
			 the bucket that contains them rather than exact. Sessions are attributed \
			 to the day they started on. `peak_concurrent` is the highest number of \
			 players online in any single hour of the period.",
		)
		.tag("analytics")
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionsResponse {
	start: NaiveDate,
	end: NaiveDate,
	sessions: i64,
	/// Interpolated from the bucketed distribution; null when no sessions.
	percentiles: SessionPercentiles,
	distribution: Vec<SessionLengthBucket>,
	peak_concurrent: Option<PeakConcurrent>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionPercentiles {
	p50_seconds: Option<i64>,
	p90_seconds: Option<i64>,
	p99_seconds: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionLengthBucket {
	min_seconds: i64,
	/// Absent on the final, open-ended bucket.
	max_seconds: Option<i64>,
	sessions: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PeakConcurrent {
	players: i32,
	hour_start: DateTime<FixedOffset>,
}

/// Interpolates within the containing bucket; approximate, since only the
/// bucket a session fell into is stored.
fn percentile_from_histogram(
	buckets: &[SessionLengthBucket],
	quantile: f64,
) -> Option<i64> {
	let total: i64 = buckets.iter().map(|bucket| bucket.sessions).sum();
	if total == 0 {
		return None;
	}

	#[expect(
		clippy::cast_precision_loss,
		clippy::cast_possible_truncation,
		reason = "session counts are far below the f64 mantissa"
	)]
	let target = ((quantile * total as f64).ceil() as i64).max(1);

	let mut cumulative = 0;
	for bucket in buckets {
		if bucket.sessions == 0 {
			continue;
		}
		if cumulative + bucket.sessions >= target {
			let Some(max_seconds) = bucket.max_seconds else {
				return Some(bucket.min_seconds);
			};

			#[expect(
				clippy::cast_precision_loss,
				reason = "bucket widths and counts are small"
			)]
			let within = (target - cumulative) as f64 / bucket.sessions as f64;
			#[expect(
				clippy::cast_precision_loss,
				clippy::cast_possible_truncation,
				reason = "bucket widths are small"
			)]
			let offset = ((max_seconds - bucket.min_seconds) as f64 * within) as i64;

			return Some(bucket.min_seconds + offset);
		}
		cumulative += bucket.sessions;
	}

	buckets.last().map(|bucket| bucket.min_seconds)
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn sessions_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<SessionsResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;

	let rows = AnalyticsSessionLengthDaily::find()
		.filter(analytics_session_length_daily::Column::Day.gte(start))
		.filter(analytics_session_length_daily::Column::Day.lte(end))
		.all(&state.database)
		.await?;

	let mut totals = [0i64; SESSION_LENGTH_BUCKETS.len()];
	for row in rows {
		if let Some(total) = usize::try_from(row.bucket)
			.ok()
			.and_then(|bucket| totals.get_mut(bucket))
		{
			*total += i64::from(row.sessions);
		}
	}

	let distribution: Vec<SessionLengthBucket> = SESSION_LENGTH_BUCKETS
		.iter()
		.enumerate()
		.map(|(index, &min_seconds)| SessionLengthBucket {
			min_seconds,
			max_seconds: SESSION_LENGTH_BUCKETS.get(index + 1).copied(),
			sessions: totals[index],
		})
		.collect();

	let peak = AnalyticsHourly::find()
		.filter(analytics_hourly::Column::HourStart.gte(start_of_day(start)))
		.filter(
			analytics_hourly::Column::HourStart.lt(end
				.succ_opt()
				.map_or_else(|| start_of_day(end), start_of_day)),
		)
		.order_by_desc(analytics_hourly::Column::PeakConcurrent)
		.one(&state.database)
		.await?
		.filter(|row| row.peak_concurrent > 0)
		.map(|row| PeakConcurrent {
			players: row.peak_concurrent,
			hour_start: row.hour_start,
		});

	Ok(Json(SessionsResponse {
		sessions: distribution.iter().map(|bucket| bucket.sessions).sum(),
		percentiles: SessionPercentiles {
			p50_seconds: percentile_from_histogram(&distribution, 0.50),
			p90_seconds: percentile_from_histogram(&distribution, 0.90),
			p99_seconds: percentile_from_histogram(&distribution, 0.99),
		},
		distribution,
		peak_concurrent: peak,
		start,
		end,
	}))
}
