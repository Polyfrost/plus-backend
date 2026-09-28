use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{NaiveDate, Utc};
use entities::{player_mod, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	EntityTrait, FromQueryResult, QueryOrder as _, QuerySelect as _, sea_query::Expr,
};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, filter_timestamp_period,
	resolve_series_bounds,
};
use crate::api::ApiState;

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ModsResponse {
	start: NaiveDate,
	end: NaiveDate,
	/// Players whose latest mod report falls in the period.
	reporting_players: i64,
	mods: Vec<ModUsage>,
}

#[derive(Debug, Serialize, JsonSchema, FromQueryResult)]
pub(super) struct ModUsage {
	mod_id: String,
	players: i64,
}

pub(super) fn mods_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsMods")
		.summary("Get installed mod usage")
		.description(
			"How many players have each mod installed, most used first.\n\nClients \
			 report their top-level mods once per launch and each report replaces the \
			 last, so a player counts in the period holding their latest report.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn mods_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<ModsResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;
	let period = AnalyticsPeriod {
		start: Some(start),
		end: Some(end),
	};
	let reported = || {
		filter_timestamp_period(
			PlayerMod::find(),
			player_mod::Column::ReportedAt,
			period,
		)
		.select_only()
	};

	let reporting_players = reported()
		.column_as(Expr::cust("COUNT(DISTINCT player_id)"), "players")
		.into_tuple::<i64>()
		.one(&state.database)
		.await?
		.unwrap_or(0);

	let mods = reported()
		.column(player_mod::Column::ModId)
		.column_as(Expr::cust("COUNT(*)"), "players")
		.group_by(player_mod::Column::ModId)
		.order_by_desc(Expr::cust("players"))
		.order_by_asc(player_mod::Column::ModId)
		.into_model::<ModUsage>()
		.all(&state.database)
		.await?;

	Ok(Json(ModsResponse {
		start,
		end,
		reporting_players,
		mods,
	}))
}
