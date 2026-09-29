use aide::{
	OperationIo,
	axum::{ApiRouter, routing::delete_with},
	transform::TransformOperation,
};
use axum::{
	extract::{Path, State},
	http::StatusCode,
	response::IntoResponse,
};
use sea_orm::{ColumnTrait as _, EntityTrait, QueryFilter as _};

use crate::api::{ApiState, admin_auth::AdminAuthenticationExtractor};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum DeleteError {
	#[error("No collection with that id")]
	NotFound,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for DeleteError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteCollection")
		.summary("Delete a collection")
		.description(
			"Deletes a collection. Cosmetics and bundles referencing it have their \
			 collection cleared. The collection's asset is left untouched. Admin role \
			 required.",
		)
		.tag("collections")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/delete/{id}",
		delete_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(id): Path<i32>,
) -> Result<StatusCode, DeleteError> {
	use entities::{discount_target, prelude::*};

	// Read first: the targets go with the collection.
	let discount_ids: Vec<i32> = DiscountTarget::find()
		.filter(discount_target::Column::CollectionId.eq(id))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|target| target.discount_id)
		.collect();

	let result = Collections::delete_by_id(id).exec(&state.database).await?;

	if result.rows_affected == 0 {
		return Err(DeleteError::NotFound);
	}

	// After the commit, and not worth failing the request over: the collection
	// is gone either way, and provisioning repairs PayNow.
	if let Err(error) = crate::storefront::collection_deleted(
		&state.database,
		&state.paynow.client,
		id,
		&discount_ids,
	)
	.await
	{
		tracing::warn!(
			collection = id,
			"Unable to update PayNow after deleting a collection; \
			 provision-paynow --sync-discounts will repair it: {error}"
		);
	}

	Ok(StatusCode::NO_CONTENT)
}
