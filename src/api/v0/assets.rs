use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::{IntoResponse, Redirect},
};
use schemars::JsonSchema;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};

use crate::api::{
	ApiState, admin_auth::AdminAuthenticationExtractor, state::AssetCacheError,
	v0::cosmetics::CachedAssetInfo,
};

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/asset/{id}",
			get_with(self::redirect_endpoint, self::redirect_doc),
		)
		.api_route(
			"/asset/{id}/url",
			get_with(self::url_endpoint, self::url_doc),
		)
		.api_route(
			"/assets/refresh",
			post_with(self::refresh_all_endpoint, self::refresh_all_doc),
		)
		.api_route(
			"/assets/refresh/{id}",
			post_with(self::refresh_one_endpoint, self::refresh_one_doc),
		)
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum AssetError {
	#[error("No asset with that id has a resolvable url")]
	NotFound,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Unable to presign asset url: {0}")]
	S3(#[from] s3::error::S3Error),
	#[error(transparent)]
	Refresh(#[from] AssetCacheError),
}

impl IntoResponse for AssetError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::Database(_) | Self::S3(_) | Self::Refresh(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AssetPath {
	id: i32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AssetUrlResponse {
	url: String,
}

/// What a full asset cache refresh did.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RefreshAllResponse {
	/// How many assets had their cached info recomputed and stored.
	refreshed: usize,
	/// How many assets could not be resolved and are therefore absent from the
	/// cache. They fall back to being resolved per request until they succeed.
	failed: usize,
	/// How many cached entries were dropped because their asset is gone.
	evicted: usize,
}

/// Resolve the direct url for the asset with the given id, presigning an S3
/// object url when the asset is backed by object storage.
async fn resolve_url(state: &ApiState, id: i32) -> Result<String, AssetError> {
	use entities::prelude::*;

	let asset = Asset::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(AssetError::NotFound)?;

	match (&asset.url, &asset.storage_path) {
		(Some(url), _) => Ok(url.clone()),
		(None, Some(path)) => Ok(state.s3_bucket.presign_get(path, 86400, None).await?), // 24h
		(None, None) => Err(AssetError::NotFound),
	}
}

fn redirect_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAsset")
		.summary("Get an asset")
		.description(
			"Redirects to the resolved url for the given asset. Prefer `/asset/{id}/url` \
			 from browser JavaScript, since following this redirect to object storage is \
			 subject to CORS. Returns 404 when no asset with that id has a resolvable url.",
		)
		.tag("assets")
}

fn url_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAssetUrl")
		.summary("Get an asset's url")
		.description(
			"Returns the resolved url for the given asset as JSON, without redirecting. \
			 Use this from browser JavaScript to fetch the asset directly and avoid the \
			 CORS pitfalls of a cross-origin redirect. Returns 404 when no asset with \
			 that id has a resolvable url.",
		)
		.tag("assets")
}

#[tracing::instrument(level = "debug", skip(state))]
async fn redirect_endpoint(
	State(state): State<ApiState>,
	Path(AssetPath { id }): Path<AssetPath>,
) -> Result<Redirect, AssetError> {
	Ok(Redirect::temporary(&resolve_url(&state, id).await?))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn url_endpoint(
	State(state): State<ApiState>,
	Path(AssetPath { id }): Path<AssetPath>,
) -> Result<Json<AssetUrlResponse>, AssetError> {
	Ok(Json(AssetUrlResponse {
		url: resolve_url(&state, id).await?,
	}))
}

fn refresh_all_doc(op: TransformOperation) -> TransformOperation {
	op.id("refreshAssetCache")
		.summary("Refresh the asset cache")
		.description(
			"Recomputes the cached hash of every asset from the database and object \
			 storage, and drops cached entries whose asset no longer exists. Cached \
			 hashes never expire on their own, so this is how a change made outside \
			 the API - an object replaced in storage, a hash edited in the database \
			 - is picked up without a restart. Runs synchronously and reports what \
			 it did. Admin password required.",
		)
		.tag("assets")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<RefreshAllResponse>, _>(|res| {
			res.description("The cache was rebuilt")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

fn refresh_one_doc(op: TransformOperation) -> TransformOperation {
	op.id("refreshAsset")
		.summary("Refresh a single asset")
		.description(
			"Recomputes the cached hash of one asset and returns it. Prefer this \
			 over a full refresh when a single object changed: it costs one lookup \
			 instead of one per asset. Returns 404 when no asset with that id \
			 exists, in which case any leftover cache entry is dropped. Admin \
			 password required.",
		)
		.tag("assets")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<CachedAssetInfo>, _>(|res| {
			res.description("The asset's cached info was recomputed")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No asset exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn refresh_all_endpoint(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
) -> Result<Json<RefreshAllResponse>, AssetError> {
	let refresh = state.refresh_asset_cache().await?;

	tracing::info!(
		refreshed = refresh.refreshed,
		failed = refresh.failed,
		evicted = refresh.evicted,
		"Refreshed the asset cache on request"
	);

	Ok(Json(RefreshAllResponse {
		refreshed: refresh.refreshed,
		failed: refresh.failed,
		evicted: refresh.evicted,
	}))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn refresh_one_endpoint(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(AssetPath { id }): Path<AssetPath>,
) -> Result<Json<CachedAssetInfo>, AssetError> {
	state
		.refresh_asset(id)
		.await?
		.map(Json)
		.ok_or(AssetError::NotFound)
}
