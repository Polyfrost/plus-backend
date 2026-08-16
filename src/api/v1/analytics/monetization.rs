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
use crate::api::ApiState;

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct MonetizationResponse {
	start: NaiveDate,
	end: NaiveDate,
	gross_revenue_minor: i64,
	refund_amount_minor: i64,
	/// Gross booked in the period minus refunds processed in it. A refund is
	/// booked on the day it was processed, not the day of the original sale.
	net_revenue_minor: i64,
	discount_amount_minor: i64,
	/// Sales made in the period, including ones refunded later.
	transactions_completed: i64,
	/// Sales made in the period that have since been refunded, whenever that
	/// refund was processed.
	transactions_refunded: i64,
	gift_transactions: i64,
	/// Distinct payers per day, summed. Not distinct across the period.
	paying_user_days: i64,
	days: Vec<MonetizationDay>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct MonetizationDay {
	day: NaiveDate,
	gross_revenue_minor: i64,
	refund_amount_minor: i64,
	discount_amount_minor: i64,
	transactions_completed: i32,
	transactions_refunded: i32,
	paying_users: i32,
	gift_transactions: i32,
}

pub(super) fn monetization_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsMonetization")
		.summary("Get revenue and refunds")
		.description(
			"Revenue in the currency's minor units.\n\nA sale stays booked on the day it \
			 was made even if it is refunded later; the refund is booked separately, on \
			 the day it was processed, so `net_revenue_minor` only subtracts it once.\
			 \n\nAmounts are only recorded from the point the Stripe webhook began \
			 storing them; earlier transactions read as zero until backfilled. Amounts \
			 are not converted between currencies, so a store selling in more than one \
			 will produce a meaningless sum.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn monetization_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<MonetizationResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;

	let days: Vec<MonetizationDay> = AnalyticsDaily::find()
		.filter(analytics_daily::Column::Day.gte(start))
		.filter(analytics_daily::Column::Day.lte(end))
		.order_by_asc(analytics_daily::Column::Day)
		.all(&state.database)
		.await?
		.into_iter()
		.map(|row| MonetizationDay {
			day: row.day,
			gross_revenue_minor: row.gross_revenue_minor,
			refund_amount_minor: row.refund_amount_minor,
			discount_amount_minor: row.discount_amount_minor,
			transactions_completed: row.transactions_completed,
			transactions_refunded: row.transactions_refunded,
			paying_users: row.paying_users,
			gift_transactions: row.gift_transactions,
		})
		.collect();

	let gross: i64 = days.iter().map(|day| day.gross_revenue_minor).sum();
	let refunds: i64 = days.iter().map(|day| day.refund_amount_minor).sum();

	Ok(Json(MonetizationResponse {
		start,
		end,
		gross_revenue_minor: gross,
		refund_amount_minor: refunds,
		net_revenue_minor: gross - refunds,
		discount_amount_minor: days.iter().map(|day| day.discount_amount_minor).sum(),
		transactions_completed: days
			.iter()
			.map(|day| i64::from(day.transactions_completed))
			.sum(),
		transactions_refunded: days
			.iter()
			.map(|day| i64::from(day.transactions_refunded))
			.sum(),
		gift_transactions: days
			.iter()
			.map(|day| i64::from(day.gift_transactions))
			.sum(),
		paying_user_days: days.iter().map(|day| i64::from(day.paying_users)).sum(),
		days,
	}))
}
