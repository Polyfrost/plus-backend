use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Serialize;
use tokio::task::JoinSet;

use crate::api::{ApiState, cosmetics::CosmeticInfo};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("Unable to fetch user data from database: {0}")]
	DatabaseFetch(#[from] sea_orm::error::DbErr),
	#[error("Unable to presign S3 URLs: {0}")]
	S3Presign(#[from] s3::error::S3Error),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("listCapes")
		.summary("List all capes")
		.description("Lists all capes, including their URLs and unique IDs")
		.tag("cosmetics")
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to fetch capes",
				)
			},
		)
}

impl IntoResponse for ResponseError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				ResponseError::S3Presign(_) => StatusCode::INTERNAL_SERVER_ERROR,
				ResponseError::DatabaseFetch(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

/// Information about the player's cosmetics
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Response {
	capes: Vec<CosmeticInfo>,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/capes", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
) -> Result<Json<Response>, ResponseError> {
	let mut response = Response::default();

	{
		use entities::{cosmetic, prelude::*};

		let cosmetics = Cosmetic::find()
			.filter(cosmetic::Column::Type.eq(CosmeticType::Cape))
			.all(&state.database)
			.await?;

		let mut join_set = JoinSet::new();
		for cosmetic in cosmetics {
			let cosmetic_cache = state.cosmetic_cache.clone();
			let s3_bucket = state.s3_bucket.clone();
			join_set.spawn(async move {
				CosmeticInfo::from_db_model(&cosmetic, cosmetic_cache, s3_bucket).await
			});
		}

		response.capes.extend(
			join_set
				.join_all()
				.await
				.into_iter()
				.collect::<Result<Vec<_>, _>>()?,
		);
	};

	Ok(Json(response))
}
