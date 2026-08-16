use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{NaiveDate, Utc};
use entities::{analytics_daily, prelude::*};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait as _, EntityTrait, QueryFilter as _, QueryOrder as _};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, resolve_series_bounds,
};
use crate::api::{ApiState, state::ANALYTICS_DAILY_JOB};

pub(super) fn daily_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsDaily")
		.summary("Get the daily analytics series")
		.description(
			"Returns one precomputed row per UTC day. `start` and `end` are optional \
			 inclusive `YYYY-MM-DD` days; `end` defaults to today and `start` to 89 days \
			 before it. Ranges longer than 1000 days are rejected.\n\nRows are produced \
			 by a background job, so the most recent days may lag. `computed_through` \
			 reports the last day the job considers final; use `/analytics/health` to \
			 check whether it is keeping up.",
		)
		.tag("analytics")
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DailyResponse {
	start: NaiveDate,
	end: NaiveDate,
	/// Last day the rollup job treats as final; later rows still move.
	computed_through: Option<NaiveDate>,
	days: Vec<DailyPoint>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DailyPoint {
	day: NaiveDate,
	dau: i32,
	wau: i32,
	mau: i32,
	new_users: i32,
	returning_users: i32,
	total_users: i32,
	playtime_seconds: i64,
	sessions: i32,
	cosmetics_acquired: i32,
	cosmetics_acquired_paid: i32,
	cosmetics_acquired_granted: i32,
	transactions_completed: i32,
	transactions_refunded: i32,
	gift_transactions: i32,
	friend_requests_sent: i32,
	friend_requests_accepted: i32,
	friendships_created: i32,
	blocks_created: i32,
	messages_sent: i32,
	game_sessions_created: i32,
	session_invites_sent: i32,
	session_invites_accepted: i32,
	tracked_link_hits: i32,
}

impl From<analytics_daily::Model> for DailyPoint {
	fn from(row: analytics_daily::Model) -> Self {
		Self {
			day: row.day,
			dau: row.dau,
			wau: row.wau,
			mau: row.mau,
			new_users: row.new_users,
			returning_users: row.returning_users,
			total_users: row.total_users,
			playtime_seconds: row.playtime_seconds,
			sessions: row.sessions,
			cosmetics_acquired: row.cosmetics_acquired,
			cosmetics_acquired_paid: row.cosmetics_acquired_paid,
			cosmetics_acquired_granted: row.cosmetics_acquired_granted,
			transactions_completed: row.transactions_completed,
			transactions_refunded: row.transactions_refunded,
			gift_transactions: row.gift_transactions,
			friend_requests_sent: row.friend_requests_sent,
			friend_requests_accepted: row.friend_requests_accepted,
			friendships_created: row.friendships_created,
			blocks_created: row.blocks_created,
			messages_sent: row.messages_sent,
			game_sessions_created: row.game_sessions_created,
			session_invites_sent: row.session_invites_sent,
			session_invites_accepted: row.session_invites_accepted,
			tracked_link_hits: row.tracked_link_hits,
		}
	}
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn daily_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<DailyResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;

	let days = AnalyticsDaily::find()
		.filter(analytics_daily::Column::Day.gte(start))
		.filter(analytics_daily::Column::Day.lte(end))
		.order_by_asc(analytics_daily::Column::Day)
		.all(&state.database)
		.await?
		.into_iter()
		.map(DailyPoint::from)
		.collect();

	let computed_through = AnalyticsJobState::find_by_id(ANALYTICS_DAILY_JOB)
		.one(&state.database)
		.await?
		.map(|state| state.finalized_through);

	Ok(Json(DailyResponse {
		start,
		end,
		computed_through,
		days,
	}))
}
