use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation
};
use axum::{
	Json,
	extract::{Query, State, rejection::QueryRejection},
	http::StatusCode,
	response::IntoResponse
};
use entities::prelude::*;
use schemars::JsonSchema;
use sea_orm::{TransactionTrait, prelude::*};
use serde::{Deserialize, Serialize};
use tebex::apis::plugin::customer_purchases::ActivePackagesRequest;
use uuid::Uuid;

use crate::{api::ApiState, database::DatabaseUserExt};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum RestoreError {
	#[error("The passed player UUID was not a valid UUID: {0}")]
	InvalidUuid(#[from] QueryRejection),
	#[error("Unable to fetch active packages for player: {0}")]
	ActivePackagesFetch(#[source] reqwest::Error),
	#[error("Unable to query database: {0}")]
	DatabaseError(#[from] sea_orm::DbErr)
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("restorePayments")
		.summary("Restore all previous payments made by a player")
		.description(
			"Fetches all payments made by a player from Tebex and ensures they are \
			 properly stored in the database"
		)
		.tag("payments")
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to restore payments"
				)
			}
		)
}

impl IntoResponse for RestoreError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::InvalidUuid(_) => StatusCode::BAD_REQUEST,
				Self::ActivePackagesFetch(_) => StatusCode::INTERNAL_SERVER_ERROR,
				Self::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR
			},
			self.to_string()
		)
			.into_response()
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreQuery {
	/// The UUID of the player to attempt to restore the purchases of
	#[schemars(example = &"f7c77d999f154a66a87dc4a51ef30d19")]
	player: Uuid
}

/// A response given on successful a payment restore
#[derive(Debug, Default, Serialize, JsonSchema)]
struct RestoreResponse {
	/// A list of all Tebex payment IDs restored
	#[schemars(example = ["tbx-42121625a15259-c182f4", "tbx-18222225a75296-bc0925"])]
	restored_ids: Vec<String>
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/restore", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	query: Result<Query<RestoreQuery>, QueryRejection>
) -> Result<Json<RestoreResponse>, RestoreError> {
	// Handle query errors
	let Query(query) = query?;

	// Fetch all active purchases for the player
	let active_packages = state
		.tebex
		.plugin_client
		.active_packages(ActivePackagesRequest {
			id: query
				.player
				.simple()
				.encode_lower(&mut Uuid::encode_buffer()),
			package: None
		})
		.await
		.map_err(RestoreError::ActivePackagesFetch)?; // TODO handle 404 "invalid ID" from tebex

	let mut response = RestoreResponse::default();

	if active_packages.is_empty() {
		return Ok(Json(response));
	}

	let txn = state.database.begin().await?;

	let user = User::get_or_create(&txn, query.player).await?;

	// Fetch all stored cosmetics for the player
	let cosmetics = user.find_related(UserCosmetic).all(&state.database).await?;

	// TODO!: tebex only returns the package ID, so we also need to fetch the
	// package to get the custom data that contains cosmetic info
	//
	// also verify my left join is actually correct because idk how that works
	for info in active_packages {
		// if info.package
	}

	Ok(Json(response))
}
