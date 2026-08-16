use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{Days, Utc};
use entities::{
	analytics_daily, monthly_active_login, player_owned_cosmetic, prelude::*, user,
};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, ConnectionTrait as _, DatabaseConnection, EntityTrait,
	FromQueryResult, PaginatorTrait as _, QueryFilter as _, QuerySelect as _,
	QueryTrait as _,
	sea_query::{Alias, Asterisk, Expr, Func, SimpleExpr},
};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, filter_date_period,
	filter_timestamp_period,
};
use crate::{api::ApiState, utils::time::current_utc_month};

/// Buckets in the database, so the result is one row however many players.
async fn owned_items_histogram(
	database: &DatabaseConnection,
	period: AnalyticsPeriod,
) -> Result<OwnedItemsCounts, sea_orm::DbErr> {
	let owned = Alias::new("owned");

	let per_player = filter_timestamp_period(
		PlayerOwnedCosmetic::find(),
		player_owned_cosmetic::Column::AcquiredAt,
		period,
	)
	.select_only()
	.column(player_owned_cosmetic::Column::PlayerId)
	.column_as(player_owned_cosmetic::Column::CosmeticId.count(), "owned")
	.group_by(player_owned_cosmetic::Column::PlayerId)
	.into_query();

	// Summing over no rows gives NULL rather than 0.
	let bucket = |condition: SimpleExpr| {
		SimpleExpr::from(Func::coalesce([
			Expr::expr(Expr::case(condition, 1).finally(0)).sum(),
			Expr::val(0i64).into(),
		]))
	};

	let query = sea_orm::sea_query::Query::select()
		.expr_as(
			Func::coalesce([
				Expr::col(owned.clone()).sum().cast_as(Alias::new("bigint")),
				Expr::val(0i64).into(),
			]),
			Alias::new("total_owned_items"),
		)
		.expr_as(Expr::col(Asterisk).count(), Alias::new("users_with_any"))
		.expr_as(bucket(Expr::col(owned.clone()).eq(1)), Alias::new("one"))
		.expr_as(
			bucket(Expr::col(owned.clone()).between(2, 5)),
			Alias::new("two_5"),
		)
		.expr_as(
			bucket(Expr::col(owned.clone()).between(6, 10)),
			Alias::new("six_10"),
		)
		.expr_as(bucket(Expr::col(owned).gte(11)), Alias::new("eleven_plus"))
		.from_subquery(per_player, Alias::new("per_player"))
		.to_owned();

	let backend = database.get_database_backend();

	Ok(OwnedItemsCounts::find_by_statement(backend.build(&query))
		.one(database)
		.await?
		.unwrap_or_default())
}

pub(super) fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsOverview")
		.summary("Get private analytics overview")
		.description(
			"Returns private aggregate analytics for users, MAU, owned items, and \
			 playtime.\n\nBoth `start` and `end` are optional inclusive `YYYY-MM-DD` UTC \
			 days, and either can be given on its own. Cumulative metrics (`total_users`, \
			 owned items and their distribution) are snapshots of the state at the end of \
			 the period, while flow metrics (`new_users`, `items_acquired`, playtime and \
			 sessions) only count activity inside the period. Without either parameter \
			 the response is the all-time overview.",
		)
		.tag("analytics")
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct AnalyticsOverviewResponse {
	period: AnalyticsPeriod,
	total_users: i64,
	new_users: i64,
	monthly_active_users: i64,
	owned_items_per_user: OwnedItemsPerUser,
	playtime: Playtime,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Playtime {
	total_seconds: i64,
	average_seconds_per_user: f64,
	last_30d_seconds: i64,
	total_sessions: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct OwnedItemsPerUser {
	total_owned_items: i64,
	average_per_user: f64,
	users_with_any: i64,
	items_acquired: i64,
	distribution: OwnedItemsDistribution,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct OwnedItemsDistribution {
	#[serde(rename = "0")]
	zero: i64,
	#[serde(rename = "1")]
	one: i64,
	#[serde(rename = "2_5")]
	two_5: i64,
	#[serde(rename = "6_10")]
	six_10: i64,
	#[serde(rename = "11_plus")]
	eleven_plus: i64,
}

#[derive(Debug, Default, FromQueryResult)]
pub(super) struct DistinctCount {
	count: i64,
}

#[derive(Debug, Default, FromQueryResult)]
pub(super) struct OwnedItemsCounts {
	total_owned_items: i64,
	users_with_any: i64,
	one: i64,
	two_5: i64,
	six_10: i64,
	eleven_plus: i64,
}

#[derive(Debug, Default, FromQueryResult)]
pub(super) struct PlaytimeAggregate {
	total_seconds: Option<i64>,
	total_sessions: Option<i64>,
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<AnalyticsOverviewResponse>, AnalyticsError> {
	let period = period.validate()?;
	let up_to_end = AnalyticsPeriod {
		start: None,
		end: period.end,
	};

	let total_users =
		filter_timestamp_period(User::find(), user::Column::CreatedAt, up_to_end)
			.count(&state.database)
			.await? as i64;
	let new_users = filter_timestamp_period(User::find(), user::Column::CreatedAt, period)
		.count(&state.database)
		.await? as i64;

	let monthly_active_users = if period.is_unbounded() {
		MonthlyActiveLogin::find()
			.filter(monthly_active_login::Column::Month.eq(current_utc_month()))
			.count(&state.database)
			.await? as i64
	} else {
		let mut query = MonthlyActiveLogin::find().select_only().column_as(
			Expr::expr(Func::count_distinct(Expr::col(
				monthly_active_login::Column::PlayerId,
			))),
			"count",
		);
		if let Some(start) = period.start_timestamp() {
			query = query.filter(monthly_active_login::Column::LastLoginAt.gte(start));
		}
		if let Some(end) = period.end_timestamp_exclusive() {
			query = query.filter(monthly_active_login::Column::FirstLoginAt.lt(end));
		}
		query
			.into_model::<DistinctCount>()
			.one(&state.database)
			.await?
			.unwrap_or_default()
			.count
	};

	let owned_counts = owned_items_histogram(&state.database, up_to_end).await?;

	let items_acquired = filter_timestamp_period(
		PlayerOwnedCosmetic::find(),
		player_owned_cosmetic::Column::AcquiredAt,
		period,
	)
	.count(&state.database)
	.await? as i64;

	let average_per_user = if total_users == 0 {
		0.0
	} else {
		owned_counts.total_owned_items as f64 / total_users as f64
	};

	// Summed from the daily rollup, not daily_playtime; days it has not
	// reached yet are missing. /analytics/health reports how far behind.
	let playtime_totals =
		filter_date_period(AnalyticsDaily::find(), analytics_daily::Column::Day, period)
			.select_only()
			.column_as(
				analytics_daily::Column::PlaytimeSeconds
					.sum()
					.cast_as(Alias::new("bigint")),
				"total_seconds",
			)
			.column_as(
				analytics_daily::Column::Sessions
					.sum()
					.cast_as(Alias::new("bigint")),
				"total_sessions",
			)
			.into_model::<PlaytimeAggregate>()
			.one(&state.database)
			.await?
			.unwrap_or_default();

	let thirty_days_ago = (Utc::now() - Days::new(30)).date_naive();
	let last_30d_seconds = AnalyticsDaily::find()
		.select_only()
		.column_as(
			analytics_daily::Column::PlaytimeSeconds
				.sum()
				.cast_as(Alias::new("bigint")),
			"total_seconds",
		)
		.filter(analytics_daily::Column::Day.gte(thirty_days_ago))
		.into_model::<PlaytimeAggregate>()
		.one(&state.database)
		.await?
		.unwrap_or_default()
		.total_seconds
		.unwrap_or(0);

	let total_playtime_seconds = playtime_totals.total_seconds.unwrap_or(0);
	let average_seconds_per_user = if total_users == 0 {
		0.0
	} else {
		total_playtime_seconds as f64 / total_users as f64
	};

	Ok(Json(AnalyticsOverviewResponse {
		period,
		total_users,
		new_users,
		monthly_active_users,
		playtime: Playtime {
			total_seconds: total_playtime_seconds,
			average_seconds_per_user,
			last_30d_seconds,
			total_sessions: playtime_totals.total_sessions.unwrap_or(0),
		},
		owned_items_per_user: OwnedItemsPerUser {
			total_owned_items: owned_counts.total_owned_items,
			average_per_user,
			users_with_any: owned_counts.users_with_any,
			items_acquired,
			distribution: OwnedItemsDistribution {
				zero: (total_users - owned_counts.users_with_any).max(0),
				one: owned_counts.one,
				two_5: owned_counts.two_5,
				six_10: owned_counts.six_10,
				eleven_plus: owned_counts.eleven_plus,
			},
		},
	}))
}
