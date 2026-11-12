mod activity;
mod catalog;
mod clients;
mod daily;
mod health;
mod monetization;
mod mods;
mod overview;
mod realtime;
mod retention;
mod sessions;

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
};
use axum::{http::StatusCode, response::IntoResponse};
use chrono::{Days, NaiveDate};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, EntityTrait, QueryFilter as _, Select,
	prelude::DateTimeWithTimeZone,
};
use serde::{Deserialize, Serialize};

use crate::{
	api::{
		ApiState,
		permissions::{Require, perm},
	},
	utils::time::start_of_day,
};

/// Days a single `/analytics/daily` response may cover.
const MAX_SERIES_DAYS: i64 = 1000;
/// Points a single hourly series may contain.
const MAX_SERIES_POINTS: i64 = 1000;
/// Days returned when the caller gives no `start`.
const DEFAULT_SERIES_DAYS: u64 = 89;

pub(super) type PrivateAnalyticsAuth = Require<{ perm::VIEW_ANALYTICS }>;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum AnalyticsError {
	#[error("Unable to query analytics data: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("`start` ({start}) is after `end` ({end})")]
	InvalidPeriod { start: NaiveDate, end: NaiveDate },
	#[error("Period spans {days} days, the maximum is {MAX_SERIES_DAYS}")]
	PeriodTooLong { days: i64 },
	#[error("Period covers {points} hourly points, the maximum is {MAX_SERIES_POINTS}")]
	TooManyPoints { points: i64 },
}

impl IntoResponse for AnalyticsError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
				Self::InvalidPeriod { .. }
				| Self::PeriodTooLong { .. }
				| Self::TooManyPoints { .. } => StatusCode::BAD_REQUEST,
			},
			self,
		)
	}
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, JsonSchema)]
pub struct AnalyticsPeriod {
	start: Option<NaiveDate>,
	end: Option<NaiveDate>,
}

impl AnalyticsPeriod {
	fn validate(self) -> Result<Self, AnalyticsError> {
		match (self.start, self.end) {
			(Some(start), Some(end)) if start > end => {
				Err(AnalyticsError::InvalidPeriod { start, end })
			}
			_ => Ok(self),
		}
	}

	fn is_unbounded(self) -> bool {
		self.start.is_none() && self.end.is_none()
	}

	fn start_timestamp(self) -> Option<DateTimeWithTimeZone> {
		self.start.map(start_of_day)
	}

	fn end_timestamp_exclusive(self) -> Option<DateTimeWithTimeZone> {
		self.end.and_then(|end| end.succ_opt()).map(start_of_day)
	}
}

fn filter_timestamp_period<E: EntityTrait>(
	mut select: Select<E>,
	column: E::Column,
	period: AnalyticsPeriod,
) -> Select<E> {
	if let Some(start) = period.start_timestamp() {
		select = select.filter(column.gte(start));
	}
	if let Some(end) = period.end_timestamp_exclusive() {
		select = select.filter(column.lt(end));
	}
	select
}

fn filter_date_period<E: EntityTrait>(
	mut select: Select<E>,
	column: E::Column,
	period: AnalyticsPeriod,
) -> Select<E> {
	if let Some(start) = period.start {
		select = select.filter(column.gte(start));
	}
	if let Some(end) = period.end {
		select = select.filter(column.lte(end));
	}
	select
}

fn resolve_series_bounds(
	period: AnalyticsPeriod,
	today: NaiveDate,
) -> Result<(NaiveDate, NaiveDate), AnalyticsError> {
	let end = period.end.unwrap_or(today);
	let start = period.start.unwrap_or_else(|| {
		end.checked_sub_days(Days::new(DEFAULT_SERIES_DAYS))
			.unwrap_or(end)
	});

	if start > end {
		return Err(AnalyticsError::InvalidPeriod { start, end });
	}

	let days = end.signed_duration_since(start).num_days() + 1;
	if days > MAX_SERIES_DAYS {
		return Err(AnalyticsError::PeriodTooLong { days });
	}

	Ok((start, end))
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/analytics/overview",
			get_with(overview::endpoint, overview::endpoint_doc),
		)
		.api_route(
			"/analytics/daily",
			get_with(daily::daily_endpoint, daily::daily_doc),
		)
		.api_route(
			"/analytics/retention",
			get_with(retention::retention_endpoint, retention::retention_doc),
		)
		.api_route(
			"/analytics/activity",
			get_with(activity::activity_endpoint, activity::activity_doc),
		)
		.api_route(
			"/analytics/sessions",
			get_with(sessions::sessions_endpoint, sessions::sessions_doc),
		)
		.api_route(
			"/analytics/catalog",
			get_with(catalog::catalog_endpoint, catalog::catalog_doc),
		)
		.api_route(
			"/analytics/monetization",
			get_with(
				monetization::monetization_endpoint,
				monetization::monetization_doc,
			),
		)
		.api_route("/analytics/mods", get_with(mods::mods_endpoint, mods::mods_doc))
		.api_route(
			"/analytics/clients",
			get_with(clients::clients_endpoint, clients::clients_doc),
		)
		.api_route(
			"/analytics/health",
			get_with(health::health_endpoint, health::health_doc),
		)
		.api_route(
			"/analytics/realtime",
			get_with(realtime::realtime_endpoint, realtime::realtime_doc),
		)
}

#[cfg(test)]
mod tests {
	use chrono::NaiveDate;
	
	use super::{AnalyticsError, AnalyticsPeriod};

	fn date(year: i32, month: u32, day: u32) -> NaiveDate {
		NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
	}

	fn period(start: Option<NaiveDate>, end: Option<NaiveDate>) -> AnalyticsPeriod {
		AnalyticsPeriod { start, end }
	}

	#[test]
	fn period_rejects_start_after_end() {
		let invalid = period(Some(date(2026, 7, 2)), Some(date(2026, 7, 1)));
		assert!(matches!(
			invalid.validate(),
			Err(AnalyticsError::InvalidPeriod { .. })
		));

		for valid in [
			period(Some(date(2026, 7, 1)), Some(date(2026, 7, 1))),
			period(Some(date(2026, 7, 1)), None),
			period(None, Some(date(2026, 7, 1))),
			period(None, None),
		] {
			assert!(valid.validate().is_ok());
		}
	}

	#[test]
	fn only_a_period_without_bounds_is_unbounded() {
		assert!(period(None, None).is_unbounded());
		assert!(!period(Some(date(2026, 7, 1)), None).is_unbounded());
		assert!(!period(None, Some(date(2026, 7, 1))).is_unbounded());
	}

	#[test]
	fn period_bounds_cover_whole_days() {
		let period = period(Some(date(2026, 7, 1)), Some(date(2026, 7, 31)));

		assert_eq!(
			period.start_timestamp().map(|ts| ts.to_rfc3339()),
			Some("2026-07-01T00:00:00+00:00".to_string())
		);
		assert_eq!(
			period.end_timestamp_exclusive().map(|ts| ts.to_rfc3339()),
			Some("2026-08-01T00:00:00+00:00".to_string())
		);
	}

	#[test]
	fn missing_period_bounds_produce_no_timestamps() {
		let period = period(None, None);

		assert!(period.start_timestamp().is_none());
		assert!(period.end_timestamp_exclusive().is_none());
	}
}
