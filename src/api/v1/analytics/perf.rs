use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use entities::{perf_daily, perf_regression, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, DbBackend, EntityTrait, FromQueryResult, QueryFilter as _,
	QueryOrder as _, Statement,
};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, resolve_series_bounds,
};
use crate::api::{
	ApiState,
	state::{ALL_VERSIONS, RETENTION_DAYS},
};

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PerfResponse {
	releases: Vec<ReleasePerf>,
	regressions: Vec<Regression>,
}

#[derive(Debug, Serialize, JsonSchema, FromQueryResult)]
pub(super) struct ReleasePerf {
	minecraft_version: String,
	loader: String,
	client_version: String,
	players: i64,
	reports: i64,
	median_fps: f64,
	low1_fps: f64,
	low01_fps: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Regression {
	minecraft_version: String,
	loader: String,
	mod_id: String,
	from_version: String,
	to_version: String,
	metric: String,
	pairs: i32,
	ratio: f64,
	solo_pairs: i32,
	solo_ratio: Option<f64>,
	detected_at: DateTime<FixedOffset>,
	updated_at: DateTime<FixedOffset>,
}

pub(super) fn perf_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsPerf")
		.summary("Get in-game performance by release")
		.description(
			"FPS per PolyPlus release over the last 30 days, and the mod changes after \
			 which the same players on the same hardware and settings lost FPS. Reports \
			 capped by vsync or a framerate limit are left out.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn perf_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
) -> Result<Json<PerfResponse>, AnalyticsError> {
	let releases = ReleasePerf::find_by_statement(Statement::from_string(
		DbBackend::Postgres,
		"SELECT minecraft_version, loader, client_version, \
		        count(DISTINCT player_id) AS players, count(*) AS reports, \
		        percentile_cont(0.5) WITHIN GROUP (ORDER BY median_fps) AS median_fps, \
		        percentile_cont(0.5) WITHIN GROUP (ORDER BY low1_fps) AS low1_fps, \
		        percentile_cont(0.5) WITHIN GROUP (ORDER BY low01_fps) AS low01_fps \
		 FROM perf_report WHERE reported_at > now() - interval '30 days' \
		   AND vsync IS NOT TRUE AND (max_fps IS NULL OR median_fps < max_fps * 0.9) \
		 GROUP BY 1, 2, 3 ORDER BY max(reported_at) DESC",
	))
	.all(&state.database)
	.await?;

	let regressions = PerfRegression::find()
		.order_by_desc(perf_regression::Column::UpdatedAt)
		.all(&state.database)
		.await?
		.into_iter()
		.map(|r| Regression {
			minecraft_version: r.minecraft_version,
			loader: r.loader,
			mod_id: r.mod_id,
			from_version: r.from_version,
			to_version: r.to_version,
			metric: r.metric,
			pairs: r.pairs,
			ratio: r.ratio,
			solo_pairs: r.solo_pairs,
			solo_ratio: r.solo_ratio,
			detected_at: r.detected_at,
			updated_at: r.updated_at,
		})
		.collect();

	Ok(Json(PerfResponse { releases, regressions }))
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PerfDailyResponse {
	start: NaiveDate,
	end: NaiveDate,
	all_versions: &'static str,
	releases: Vec<ReleaseDay>,
	days: Vec<PerfDay>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PerfDay {
	day: NaiveDate,
	minecraft_version: String,
	players: i32,
	median_fps: f64,
	low1_fps: f64,
	low01_fps: f64,
}

#[derive(Debug, Serialize, JsonSchema, FromQueryResult)]
pub(super) struct ReleaseDay {
	client_version: String,
	day: NaiveDate,
}

pub(super) fn perf_daily_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsPerfDaily")
		.summary("Get the daily FPS series")
		.description(
			"One point per UTC day and Minecraft version, plus one covering every \
			 version, from an hourly rollup that is kept after raw reports expire. \
			 Reports capped by vsync or a framerate limit are left out. `start` and \
			 `end` work as on `/analytics/daily`.\n\nThe series follows whoever played \
			 that day, so a shift in the player base moves it too; `/analytics/perf` \
			 compares the same players instead.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn perf_daily_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<PerfDailyResponse>, AnalyticsError> {
	let (start, end) = resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;

	let days = PerfDaily::find()
		.filter(perf_daily::Column::Day.between(start, end))
		.order_by_asc(perf_daily::Column::Day)
		.order_by_asc(perf_daily::Column::MinecraftVersion)
		.all(&state.database)
		.await?
		.into_iter()
		.map(|row| PerfDay {
			day: row.day,
			minecraft_version: row.minecraft_version,
			players: row.players,
			median_fps: row.median_fps,
			low1_fps: row.low1_fps,
			low01_fps: row.low01_fps,
		})
		.collect();

	let releases = ReleaseDay::find_by_statement(Statement::from_sql_and_values(
		DbBackend::Postgres,
		"SELECT client_version, (min(reported_at) AT TIME ZONE 'UTC')::date AS day \
		 FROM perf_report GROUP BY client_version \
		 HAVING (min(reported_at) AT TIME ZONE 'UTC')::date BETWEEN $1 AND $2 \
		    AND min(reported_at) > now() - make_interval(days => $3) \
		 ORDER BY day",
		[start.into(), end.into(), (RETENTION_DAYS - 1).into()],
	))
	.all(&state.database)
	.await?;

	Ok(Json(PerfDailyResponse {
		start,
		end,
		all_versions: ALL_VERSIONS,
		releases,
		days,
	}))
}
