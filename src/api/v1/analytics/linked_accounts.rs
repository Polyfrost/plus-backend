use std::collections::HashMap;

use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{NaiveDate, Utc};
use entities::{linked_account, prelude::*, sea_orm_active_enums::LinkProvider};
use schemars::JsonSchema;
use sea_orm::{
	DatabaseConnection, EntityTrait, FromQueryResult, Iterable as _,
	QuerySelect as _,
	sea_query::{Alias, Expr, Func, SimpleExpr},
};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, filter_timestamp_period,
	resolve_series_bounds,
};
use crate::api::ApiState;

#[derive(Debug, Default, Serialize, JsonSchema, FromQueryResult)]
pub(super) struct LinkedAccountTotals {
	/// Links held at the end of the period.
	total_links: i64,
	/// Users holding at least one link at the end of the period.
	linked_users: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct LinkedAccountsResponse {
	start: NaiveDate,
	end: NaiveDate,
	#[serde(flatten)]
	totals: LinkedAccountTotals,
	new_links: i64,
	providers: Vec<ProviderLinks>,
	/// One entry per day of the period, gaps included.
	days: Vec<LinkDay>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ProviderLinks {
	provider: LinkProvider,
	/// Held at the end of the period.
	links: i64,
	new_links: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct LinkDay {
	day: NaiveDate,
	new_links: i64,
	/// Held at the end of the day.
	total_links: i64,
}

/// Unlinking deletes the row, so these are links still held today that
/// already existed at the end of the period.
pub(super) async fn linked_account_totals(
	database: &DatabaseConnection,
	end: Option<NaiveDate>,
) -> Result<LinkedAccountTotals, sea_orm::DbErr> {
	let up_to_end = AnalyticsPeriod { start: None, end };

	Ok(filter_timestamp_period(
		LinkedAccount::find(),
		linked_account::Column::LinkedAt,
		up_to_end,
	)
	.select_only()
	.column_as(Expr::col(linked_account::Column::UserId).count(), "total_links")
	.column_as(
		Expr::col(linked_account::Column::UserId).count_distinct(),
		"linked_users",
	)
	.into_model::<LinkedAccountTotals>()
	.one(database)
	.await?
	.unwrap_or_default())
}

async fn links_by_provider(
	database: &DatabaseConnection,
	period: AnalyticsPeriod,
) -> Result<HashMap<LinkProvider, i64>, sea_orm::DbErr> {
	Ok(filter_timestamp_period(
		LinkedAccount::find(),
		linked_account::Column::LinkedAt,
		period,
	)
	.select_only()
	.column(linked_account::Column::Provider)
	.column_as(Expr::col(linked_account::Column::UserId).count(), "links")
	.group_by(linked_account::Column::Provider)
	.into_tuple::<(LinkProvider, i64)>()
	.all(database)
	.await?
	.into_iter()
	.collect())
}

pub(super) fn linked_accounts_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsLinkedAccounts")
		.summary("Get linked account usage")
		.description(
			"External accounts linked to players, per provider and per UTC day.\n\n\
			 Unlinking deletes the link, so every figure only counts links that are \
			 still held: totals are the links that existed at the end of the period, \
			 and `new_links` the ones made inside it.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn linked_accounts_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<LinkedAccountsResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;
	let period = AnalyticsPeriod {
		start: Some(start),
		end: Some(end),
	};

	let totals = linked_account_totals(&state.database, Some(end)).await?;
	let held = links_by_provider(
		&state.database,
		AnalyticsPeriod {
			start: None,
			end: Some(end),
		},
	)
	.await?;
	let made = links_by_provider(&state.database, period).await?;

	let providers: Vec<ProviderLinks> = LinkProvider::iter()
		.map(|provider| ProviderLinks {
			links: held.get(&provider).copied().unwrap_or(0),
			new_links: made.get(&provider).copied().unwrap_or(0),
			provider,
		})
		.collect();
	let new_links = providers.iter().map(|provider| provider.new_links).sum();

	let day = SimpleExpr::from(
		Func::cust(Alias::new("timezone"))
			.arg("UTC")
			.arg(Expr::col(linked_account::Column::LinkedAt)),
	)
	.cast_as(Alias::new("date"));

	let per_day: HashMap<NaiveDate, i64> = filter_timestamp_period(
		LinkedAccount::find(),
		linked_account::Column::LinkedAt,
		period,
	)
	.select_only()
	.column_as(day, "day")
	.column_as(Expr::col(linked_account::Column::UserId).count(), "links")
	.group_by(Expr::col(Alias::new("day")))
	.into_tuple::<(NaiveDate, i64)>()
	.all(&state.database)
	.await?
	.into_iter()
	.collect();

	let mut running = totals.total_links - new_links;
	let days = start
		.iter_days()
		.take_while(|day| *day <= end)
		.map(|day| {
			let new_links = per_day.get(&day).copied().unwrap_or(0);
			running += new_links;
			LinkDay {
				day,
				new_links,
				total_links: running,
			}
		})
		.collect();

	Ok(Json(LinkedAccountsResponse {
		start,
		end,
		totals,
		new_links,
		providers,
		days,
	}))
}
