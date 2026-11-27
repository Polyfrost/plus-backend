use aide::{
	OperationInput, OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State},
	http::StatusCode,
	response::IntoResponse,
};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde::Serialize;

use super::create::{UploadError, store_cover};
use crate::api::{
	ApiState,
	permissions::{Require, perm},
	v0::cosmetics::limits::{MAX_COVER_BYTES, SET_COVER_BODY_LIMIT},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SetCoverError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("Missing or invalid cosmetic_id")]
	InvalidId,
	#[error("clear_preview must be true or false")]
	InvalidClearPreview,
	#[error(transparent)]
	Upload(#[from] UploadError),
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Multipart error: {0}")]
	Multipart(#[from] axum::extract::multipart::MultipartError),
}

impl IntoResponse for SetCoverError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::Upload(error) => return error.into_response(),
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::InvalidId | Self::InvalidClearPreview => StatusCode::BAD_REQUEST,
				Self::Multipart(ref e) => e.status(),
				Self::Database(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self,
		)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
struct SetCoverResponse {
	cosmetic_id: i32,
	cover_asset_id: i32,
	preview_asset_id: Option<i32>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("setCosmeticCover")
		.summary("Set a cosmetic's cover")
		.description(
			"Stores an uploaded PNG as the cover of one cosmetic variant, replacing \
			 any rendered cover. Send `clear_preview=true` to also drop the animated \
			 preview, for one rendered from an older asset. Requires `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<SetCoverResponse>, _>(|res| {
			res.description("The cover was stored")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No cosmetic exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Missing or invalid credentials, or lacking the permission")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/set-cover", post_with(self::endpoint, self::endpoint_doc))
		.layer(DefaultBodyLimit::max(SET_COVER_BODY_LIMIT))
}

struct FileUpload(Multipart);

impl<S> FromRequest<S> for FileUpload
where
	S: Send + Sync,
{
	type Rejection = UploadError;

	async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
		Ok(Self(
			Multipart::from_request(req, state)
				.await
				.map_err(UploadError::Rejection)?,
		))
	}
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct SetCoverRequest {
	cosmetic_id: i32,
	#[schemars(with = "String")]
	file: String,
	clear_preview: Option<bool>,
}

impl OperationInput for FileUpload {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.request_body = Some(aide::openapi::ReferenceOr::Item(
			aide::openapi::RequestBody {
				description: Some("Multipart cosmetic cover upload".into()),
				content: [(
					"multipart/form-data".into(),
					aide::openapi::MediaType {
						schema: Some(aide::openapi::SchemaObject {
							json_schema: ctx.schema.subschema_for::<SetCoverRequest>(),
							example: None,
							external_docs: None,
						}),
						..Default::default()
					},
				)]
				.into_iter()
				.collect(),
				required: true,
				extensions: Default::default(),
			},
		));
	}
}

async fn endpoint(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	FileUpload(mut multipart): FileUpload,
) -> Result<Json<SetCoverResponse>, SetCoverError> {
	use entities::{cosmetic, prelude::*};

	let mut cosmetic_id = None;
	let mut file_data = None;
	let mut clear_preview = false;

	while let Some(field) = multipart.next_field().await? {
		match field.name() {
			Some("cosmetic_id") => {
				cosmetic_id = field.text().await?.trim().parse::<i32>().ok();
			}
			Some("clear_preview") => {
				clear_preview = parse_flag(&field.text().await?)?;
			}
			Some("file") => {
				file_data = Some(field.bytes().await?);
			}
			_ => {}
		}
	}

	let cosmetic_id = cosmetic_id.ok_or(SetCoverError::InvalidId)?;
	let data = file_data.ok_or(UploadError::MissingFile)?;
	if data.len() > MAX_COVER_BYTES {
		return Err(UploadError::CoverTooLarge(data.len()).into());
	}

	let Some(cosmetic) = Cosmetic::find_by_id(cosmetic_id)
		.one(&state.database)
		.await?
	else {
		return Err(SetCoverError::MissingCosmetic);
	};

	let cover_asset_id = store_cover(&state, &data).await?;

	let mut active: cosmetic::ActiveModel = cosmetic.into();
	active.cover_asset_id = Set(Some(cover_asset_id));
	if clear_preview {
		active.preview_asset_id = Set(None);
	}
	let updated = active.update(&state.database).await?;

	crate::api::v0::cosmetics::notify_catalog_updated(&state);

	Ok(Json(SetCoverResponse {
		cosmetic_id: updated.id,
		cover_asset_id,
		preview_asset_id: updated.preview_asset_id,
	}))
}

fn parse_flag(value: &str) -> Result<bool, SetCoverError> {
	match value.trim() {
		"" | "false" | "0" => Ok(false),
		"true" | "1" => Ok(true),
		_ => Err(SetCoverError::InvalidClearPreview),
	}
}

#[cfg(test)]
mod tests {
	use super::parse_flag;

	#[test]
	fn clear_preview_reads_common_booleans() {
		assert!(parse_flag("true").expect("true"));
		assert!(parse_flag(" 1 ").expect("one"));
		assert!(!parse_flag("false").expect("false"));
		assert!(!parse_flag("").expect("empty"));
		assert!(parse_flag("yes please").is_err());
	}
}
