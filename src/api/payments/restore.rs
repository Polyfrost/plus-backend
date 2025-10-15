use std::collections::{HashMap, HashSet};

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
use entities::{cosmetic_package, prelude::*, user_cosmetic};
use migrations::OnConflict;
use schemars::JsonSchema;
use sea_orm::{ActiveValue, TransactionTrait, TryInsertResult, prelude::*};
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
	restored_ids: HashSet<String>
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

	let transactions = active_packages.iter().fold(HashMap::new(), |mut acc, ap| {
		acc.insert(ap.package.id, ap.txn_id.as_str());

		acc
	});

	let txn = state.database.begin().await?;

	// Fetch all cosmetics for the active packages of this player
	let cosmetics = Cosmetic::find()
		.find_with_related(CosmeticPackage)
		.filter(cosmetic_package::Column::PackageId.is_in(transactions.keys().copied()))
		.filter(
			// sea-orm doesn't support inner joins on find_with_related
			cosmetic_package::Column::CosmeticId.is_not_null()
		)
		.all(&txn)
		.await?;

	if cosmetics.is_empty() {
		return Ok(Json(response));
	}

	// Get the user in the database (or make it if it does not exist)
	let user = User::get_or_create(&txn, query.player).await?;

	// Insert all of the cosmetics they SHOULD have into the database, ignoring
	// conflicts
	let inserted = UserCosmetic::insert_many(cosmetics.iter().map(|(c, cp)| {
		user_cosmetic::ActiveModel {
			user: ActiveValue::Set(user.id),
			cosmetic: ActiveValue::Set(c.id),
			transaction_id: ActiveValue::Set(
				transactions[&cp
					.first()
					.expect("should be at least one CosmeticPackage")
					.package_id
					.try_into()
					.expect("package_id should not be negative")]
					.to_string()
			),
			..Default::default()
		}
	}))
	.on_conflict(OnConflict::new().do_nothing().to_owned())
	.do_nothing()
	.exec_with_returning_many(&txn)
	.await?;

	txn.commit().await?;

	if let TryInsertResult::Inserted(inserted) = inserted {
		for model in inserted {
			response.restored_ids.insert(model.transaction_id);
		}
	}

	Ok(Json(response))
}
