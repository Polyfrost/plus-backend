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
	chargeback_amount_minor: i64,
	/// Gross booked in the period minus refunds and chargebacks processed in
	/// it. Both are booked on the day they were processed, not the day of the
	/// original sale.
	net_revenue_minor: i64,
	discount_amount_minor: i64,
	/// Sales made in the period, including ones refunded later.
	transactions_completed: i64,
	/// Sales made in the period that have since been refunded, whenever that
	/// refund was processed.
	transactions_refunded: i64,
	/// Sales refunded in part only, so some of their cosmetics are still owned.
	transactions_partially_refunded: i64,
	/// Sales the buyer disputed with their bank.
	transactions_charged_back: i64,
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
	chargeback_amount_minor: i64,
	discount_amount_minor: i64,
	transactions_completed: i32,
	transactions_refunded: i32,
	transactions_partially_refunded: i32,
	transactions_charged_back: i32,
	paying_users: i32,
	gift_transactions: i32,
}

pub(super) fn monetization_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsMonetization")
		.summary("Get revenue and refunds")
		.description(
			"Revenue in the currency's minor units.\n\nA sale stays booked on the day it \
			 was made even if it is refunded later; the refund is booked separately, on \
			 the day it was processed, so `net_revenue_minor` only subtracts it once. \
			 A partial refund only subtracts the part that was returned. Chargebacks \
			 are tracked separately from refunds and subtracted the same way.\
			 \n\nAmounts are not converted between currencies, so a store selling in \
			 more than one will produce a meaningless sum.",
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
			chargeback_amount_minor: row.chargeback_amount_minor,
			discount_amount_minor: row.discount_amount_minor,
			transactions_completed: row.transactions_completed,
			transactions_refunded: row.transactions_refunded,
			transactions_partially_refunded: row.transactions_partially_refunded,
			transactions_charged_back: row.transactions_charged_back,
			paying_users: row.paying_users,
			gift_transactions: row.gift_transactions,
		})
		.collect();

	let gross: i64 = days.iter().map(|day| day.gross_revenue_minor).sum();
	let refunds: i64 = days.iter().map(|day| day.refund_amount_minor).sum();
	let chargebacks: i64 = days.iter().map(|day| day.chargeback_amount_minor).sum();

	Ok(Json(MonetizationResponse {
		start,
		end,
		gross_revenue_minor: gross,
		refund_amount_minor: refunds,
		chargeback_amount_minor: chargebacks,
		net_revenue_minor: gross - refunds - chargebacks,
		discount_amount_minor: days.iter().map(|day| day.discount_amount_minor).sum(),
		transactions_completed: days
			.iter()
			.map(|day| i64::from(day.transactions_completed))
			.sum(),
		transactions_refunded: days
			.iter()
			.map(|day| i64::from(day.transactions_refunded))
			.sum(),
		transactions_partially_refunded: days
			.iter()
			.map(|day| i64::from(day.transactions_partially_refunded))
			.sum(),
		transactions_charged_back: days
			.iter()
			.map(|day| i64::from(day.transactions_charged_back))
			.sum(),
		gift_transactions: days
			.iter()
			.map(|day| i64::from(day.gift_transactions))
			.sum(),
		paying_user_days: days.iter().map(|day| i64::from(day.paying_users)).sum(),
		days,
	}))
}
