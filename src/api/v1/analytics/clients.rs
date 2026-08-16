use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use chrono::{NaiveDate, Utc};
use entities::{analytics_client_daily, prelude::*};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait as _, EntityTrait, QueryFilter as _};
use serde::Serialize;

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, resolve_series_bounds,
};
use crate::api::ApiState;

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ClientsResponse {
	start: NaiveDate,
	end: NaiveDate,
	client_versions: Vec<ClientBreakdown>,
	minecraft_versions: Vec<ClientBreakdown>,
	loaders: Vec<ClientBreakdown>,
	operating_systems: Vec<ClientBreakdown>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ClientBreakdown {
	/// Empty when the client did not report this field.
	value: String,
	/// Summed across days, so a player active on several days counts once per
	/// day rather than once overall.
	player_days: i64,
}

fn rank(counts: std::collections::HashMap<String, i64>) -> Vec<ClientBreakdown> {
	let mut ranked: Vec<ClientBreakdown> = counts
		.into_iter()
		.map(|(value, player_days)| ClientBreakdown { value, player_days })
		.collect();
	ranked.sort_by(|a, b| {
		b.player_days
			.cmp(&a.player_days)
			.then_with(|| a.value.cmp(&b.value))
	});
	ranked
}

pub(super) fn clients_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsClients")
		.summary("Get client version and platform breakdown")
		.description(
			"Active players by reported client version, Minecraft version, mod loader \
			 and operating system.\n\nThese are reported by the client at login and are \
			 optional, so an empty `value` means the client did not send that field. \
			 Counts are player-days: a player active on five days contributes five.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn clients_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(period): Query<AnalyticsPeriod>,
) -> Result<Json<ClientsResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(period.validate()?, Utc::now().date_naive())?;

	let rows = AnalyticsClientDaily::find()
		.filter(analytics_client_daily::Column::Day.gte(start))
		.filter(analytics_client_daily::Column::Day.lte(end))
		.all(&state.database)
		.await?;

	let mut client_versions = std::collections::HashMap::new();
	let mut minecraft_versions = std::collections::HashMap::new();
	let mut loaders = std::collections::HashMap::new();
	let mut operating_systems = std::collections::HashMap::new();

	for row in rows {
		let players = i64::from(row.active_players);
		*client_versions.entry(row.client_version).or_insert(0) += players;
		*minecraft_versions.entry(row.minecraft_version).or_insert(0) += players;
		*loaders.entry(row.loader).or_insert(0) += players;
		*operating_systems.entry(row.os).or_insert(0) += players;
	}

	Ok(Json(ClientsResponse {
		start,
		end,
		client_versions: rank(client_versions),
		minecraft_versions: rank(minecraft_versions),
		loaders: rank(loaders),
		operating_systems: rank(operating_systems),
	}))
}
