use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	extract::{Path, State},
	http::StatusCode,
	response::{IntoResponse, Redirect},
};
use sea_orm::EntityTrait;

use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum AssetError {
	#[error("No asset with that id has a resolvable url")]
	NotFound,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Unable to presign asset url: {0}")]
	S3(#[from] s3::error::S3Error),
}

impl IntoResponse for AssetError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::Database(_) | Self::S3(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAsset")
		.summary("Get an asset")
		.description(
			"Redirects to the S3 url for the given asset. Returns 404 when no asset \
			 with that id has a resolvable url.",
		)
		.tag("assets")
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/asset/{id}", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	Path(id): Path<i32>,
) -> Result<Redirect, AssetError> {
	use entities::prelude::*;

	let asset = Asset::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(AssetError::NotFound)?;

	let url = match (&asset.url, &asset.storage_path) {
		(Some(url), _) => url.clone(),
		(None, Some(path)) => state.s3_bucket.presign_get(path, 604800, None).await?, // 7d
		(None, None) => return Err(AssetError::NotFound),
	};

	Ok(Redirect::temporary(&url))
}
