use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use chrono::{DateTime, FixedOffset};
use entities::{player_set_discovery, prelude::*};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder};
use serde::Serialize;

use crate::api::{ApiState, error_response, v0::account::AuthenticatedPlayer};

#[derive(thiserror::Error, Debug, OperationIo)]
pub(crate) enum DiscoveredError {
	#[error("Unable to query database: {0}")]
	Database(#[from] DbErr),
}

impl IntoResponse for DiscoveredError {
	fn into_response(self) -> Response {
		error_response(StatusCode::INTERNAL_SERVER_ERROR, self)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
struct DiscoveredBonus {
	bonus_id: i32,
	name: String,
	cosmetic_ids: Vec<i32>,
	discovered_at: DateTime<FixedOffset>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("listDiscoveredBonuses")
		.summary("List the bonuses you have found")
		.description(
			"Lists the bonuses the authenticated player has unlocked at least once, \
			 with the cosmetics they wore when they found each one. Bonuses they \
			 have not found yet are never listed.",
		)
		.tag("cosmetics")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/discovered", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<DiscoveredBonus>>, DiscoveredError> {
	let rows = PlayerSetDiscovery::find()
		.filter(player_set_discovery::Column::PlayerId.eq(player.id))
		.find_also_related(CosmeticSet)
		.order_by_asc(player_set_discovery::Column::DiscoveredAt)
		.order_by_asc(player_set_discovery::Column::SetId)
		.all(&state.database)
		.await?;

	Ok(Json(
		rows.into_iter()
			.filter_map(|(discovery, set)| {
				Some(DiscoveredBonus {
					bonus_id: discovery.set_id,
					name: set?.name,
					cosmetic_ids: discovery.cosmetic_ids,
					discovered_at: discovery.discovered_at,
				})
			})
			.collect(),
	))
}
