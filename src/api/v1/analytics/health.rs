use aide::transform::TransformOperation;
use axum::{Json, extract::State};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use entities::prelude::*;
use schemars::JsonSchema;
use sea_orm::{EntityTrait, PaginatorTrait as _};
use serde::Serialize;

use super::{AnalyticsError, PrivateAnalyticsAuth};
use crate::api::{ApiState, state::ANALYTICS_DAILY_JOB};

pub(super) fn health_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsHealth")
		.summary("Get analytics rollup health")
		.description(
			"Reports how current the precomputed analytics are. A growing \
			 `watermark_age_days` or a non-null `last_error` means the rollup job is \
			 behind and the other endpoints are serving stale numbers.",
		)
		.tag("analytics")
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct HealthResponse {
	finalized_through: Option<NaiveDate>,
	/// Days between the watermark and today. Steady state is `LATE_ARRIVAL_DAYS`.
	watermark_age_days: Option<i64>,
	last_run_at: Option<DateTime<FixedOffset>>,
	last_run_ms: Option<i32>,
	last_error: Option<String>,
	rolled_up_days: u64,
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn health_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
) -> Result<Json<HealthResponse>, AnalyticsError> {
	let job = AnalyticsJobState::find_by_id(ANALYTICS_DAILY_JOB)
		.one(&state.database)
		.await?;
	let rolled_up_days = AnalyticsDaily::find().count(&state.database).await?;
	let today = Utc::now().date_naive();

	Ok(Json(HealthResponse {
		finalized_through: job.as_ref().map(|job| job.finalized_through),
		watermark_age_days: job.as_ref().map(|job| {
			today
				.signed_duration_since(job.finalized_through)
				.num_days()
		}),
		last_run_at: job.as_ref().and_then(|job| job.last_run_at),
		last_run_ms: job.as_ref().and_then(|job| job.last_run_ms),
		last_error: job.and_then(|job| job.last_error),
		rolled_up_days,
	}))
}
