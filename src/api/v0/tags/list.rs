use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::TagType;
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Serialize;

use super::TagInfo;
use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ListError {
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for ListError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListResponse {
	tags: Vec<TagInfo>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("listTags")
		.summary("List all tags")
		.description("Lists every tag.")
		.tag("tags")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/list", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
) -> Result<Json<ListResponse>, ListError> {
	use entities::prelude::*;

	let tags = Tags::find()
		.filter(entities::tags::Column::TagType.ne(TagType::Category))
		.all(&state.database)
		.await?
		.into_iter()
		.map(TagInfo::from_tag)
		.collect();

	Ok(Json(ListResponse { tags }))
}
