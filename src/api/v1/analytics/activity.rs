use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use entities::{analytics_hourly, prelude::*};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait as _, EntityTrait, QueryFilter as _, QueryOrder as _};
use serde::{Deserialize, Serialize};

use super::{
	AnalyticsError, AnalyticsPeriod, MAX_SERIES_POINTS, PrivateAnalyticsAuth,
	resolve_series_bounds,
};
use crate::{api::ApiState, utils::time::start_of_day};

pub(super) fn activity_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsActivity")
		.summary("Get activity by hour")
		.description(
			"Answers when players are actually online.\n\n`shape=heatmap` (the default) \
			 returns 168 buckets, one per ISO weekday and UTC hour, aggregated over the \
			 period. `shape=series` returns one point per hour instead, which is capped \
			 at 1000 points (about 41 days).\n\nAll hours are UTC.",
		)
		.tag("analytics")
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum ActivityShape {
	#[default]
	Heatmap,
	Series,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ActivityQuery {
	#[serde(flatten)]
	period: AnalyticsPeriod,
	#[serde(default)]
	shape: ActivityShape,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ActivityResponse {
	start: NaiveDate,
	end: NaiveDate,
	/// Populated when `shape=heatmap`.
	heatmap: Option<Vec<HeatmapCell>>,
	/// Populated when `shape=series`.
	series: Option<Vec<ActivityPoint>>,
	/// The busiest bucket by active players, if there was any activity.
	busiest: Option<HeatmapCell>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct HeatmapCell {
	/// ISO weekday, 1 = Monday.
	dow: i16,
	/// UTC hour, 0-23.
	hour_of_day: i16,
	playtime_seconds: i64,
	/// Summed across the weeks in the period, so not a distinct player count.
	active_players: i64,
	sessions_started: i64,
	peak_concurrent: i32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ActivityPoint {
	hour_start: DateTime<FixedOffset>,
	playtime_seconds: i64,
	active_players: i32,
	sessions_started: i32,
	peak_concurrent: i32,
}

fn fold_heatmap(rows: &[analytics_hourly::Model]) -> Vec<HeatmapCell> {
	let mut cells: Vec<HeatmapCell> = (1..=7)
		.flat_map(|dow| {
			(0..24).map(move |hour_of_day| HeatmapCell {
				dow,
				hour_of_day,
				playtime_seconds: 0,
				active_players: 0,
				sessions_started: 0,
				peak_concurrent: 0,
			})
		})
		.collect();

	for row in rows {
		let dow = row.dow.clamp(1, 7);
		let hour = row.hour_of_day.clamp(0, 23);
		let Some(cell) = cells.get_mut(usize::from((dow - 1) as u16 * 24 + hour as u16))
		else {
			continue;
		};

		cell.playtime_seconds += row.playtime_seconds;
		cell.active_players += i64::from(row.active_players);
		cell.sessions_started += i64::from(row.sessions_started);
		cell.peak_concurrent = cell.peak_concurrent.max(row.peak_concurrent);
	}

	cells
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn activity_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(query): Query<ActivityQuery>,
) -> Result<Json<ActivityResponse>, AnalyticsError> {
	let today = Utc::now().date_naive();
	let (start, end) = resolve_series_bounds(query.period.validate()?, today)?;

	// A heatmap is 168 buckets however wide the window; a series is not.
	if matches!(query.shape, ActivityShape::Series) {
		let hours = (end.signed_duration_since(start).num_days() + 1) * 24;
		if hours > MAX_SERIES_POINTS {
			return Err(AnalyticsError::TooManyPoints { points: hours });
		}
	}

	let rows = AnalyticsHourly::find()
		.filter(analytics_hourly::Column::HourStart.gte(start_of_day(start)))
		.filter(
			analytics_hourly::Column::HourStart.lt(end
				.succ_opt()
				.map_or_else(|| start_of_day(end), start_of_day)),
		)
		.order_by_asc(analytics_hourly::Column::HourStart)
		.all(&state.database)
		.await?;

	let heatmap = fold_heatmap(&rows);
	let busiest = heatmap
		.iter()
		.max_by_key(|cell| cell.active_players)
		.filter(|cell| cell.active_players > 0)
		.cloned();

	let (heatmap, series) = match query.shape {
		ActivityShape::Heatmap => (Some(heatmap), None),
		ActivityShape::Series => (
			None,
			Some(
				rows.into_iter()
					.map(|row| ActivityPoint {
						hour_start: row.hour_start,
						playtime_seconds: row.playtime_seconds,
						active_players: row.active_players,
						sessions_started: row.sessions_started,
						peak_concurrent: row.peak_concurrent,
					})
					.collect(),
			),
		),
	};

	Ok(Json(ActivityResponse {
		start,
		end,
		heatmap,
		series,
		busiest,
	}))
}
