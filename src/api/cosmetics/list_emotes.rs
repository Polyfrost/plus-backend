use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use schemars::JsonSchema;
use sea_orm::EntityTrait;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::api::{ApiState, cosmetics::EmoteInfo};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("Unable to fetch emotes from database: {0}")]
	DatabaseFetch(#[from] sea_orm::error::DbErr),
	#[error("Unable to presign S3 URLs: {0}")]
	S3Presign(#[from] s3::error::S3Error),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("listEmotes")
		.summary("List all emotes")
		.description("Lists all emotes, including their URLs and unique IDs")
		.tag("emotes")
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to fetch emotes",
				)
			},
		)
}

impl IntoResponse for ResponseError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				ResponseError::S3Presign(_) | ResponseError::DatabaseFetch(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self.to_string(),
		)
			.into_response()
	}
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Response {
	emotes: Vec<EmoteInfo>,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/emotes", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
) -> Result<Json<Response>, ResponseError> {
	let mut response = Response::default();

	{
		use entities::prelude::*;

		let emotes = Emote::find()
			.find_also_related(Asset)
			.all(&state.database)
			.await?;

		let mut join_set = JoinSet::new();
		for (emote, asset) in emotes {
			let asset_cache = state.asset_cache.clone();
			let s3_bucket = state.s3_bucket.clone();
			join_set.spawn(async move {
				EmoteInfo::from_db_model(&emote, asset.as_ref(), asset_cache, s3_bucket)
					.await
			});
		}

		response.emotes.extend(
			join_set
				.join_all()
				.await
				.into_iter()
				.collect::<Result<Vec<_>, _>>()?,
		);
	};

	Ok(Json(response))
}
