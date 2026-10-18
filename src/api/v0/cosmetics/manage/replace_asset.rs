use aide::{
	OperationInput, OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{FromRequest, Multipart, Request, State},
	http::StatusCode,
	response::IntoResponse,
};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde::Serialize;

use super::create::{UploadError, store_cosmetic_file};
use crate::api::{ApiState, admin_auth::AdminAuthenticationExtractor};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ReplaceAssetError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("Missing or invalid cosmetic_id")]
	InvalidId,
	#[error(transparent)]
	Upload(#[from] UploadError),
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Multipart error: {0}")]
	Multipart(#[from] axum::extract::multipart::MultipartError),
}

impl IntoResponse for ReplaceAssetError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::Upload(error) => return error.into_response(),
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::InvalidId => StatusCode::BAD_REQUEST,
				Self::Database(_) | Self::Multipart(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self,
		)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReplaceAssetResponse {
	cosmetic_id: i32,
	asset_id: i32,
	cover_asset_id: Option<i32>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("replaceCosmeticAsset")
		.summary("Replace a cosmetic's file")
		.description(
			"Uploads a new model, texture, or emote bundle for an existing cosmetic \
			 (or a single variant) and re-renders its cover, keeping its id, \
			 ownership, and product. The previous asset is left in storage. Admin \
			 password required.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<ReplaceAssetResponse>, _>(
			|res| res.description("The file was replaced"),
		)
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No cosmetic exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/replace-asset",
		post_with(self::endpoint, self::endpoint_doc),
	)
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
struct ReplaceAssetRequest {
	/// The id of the cosmetic (variant) whose file to replace.
	cosmetic_id: i32,
	#[schemars(with = "String")]
	file: String,
}

impl OperationInput for FileUpload {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.request_body = Some(aide::openapi::ReferenceOr::Item(
			aide::openapi::RequestBody {
				description: Some("Multipart cosmetic file replacement".into()),
				content: [(
					"multipart/form-data".into(),
					aide::openapi::MediaType {
						schema: Some(aide::openapi::SchemaObject {
							json_schema: ctx
								.schema
								.subschema_for::<ReplaceAssetRequest>(),
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
	_auth: AdminAuthenticationExtractor,
	FileUpload(mut multipart): FileUpload,
) -> Result<Json<ReplaceAssetResponse>, ReplaceAssetError> {
	use entities::{cosmetic, prelude::*};

	let mut cosmetic_id = None;
	let mut file_data = None;
	let mut content_type = None;
	let mut extension = "png".to_string();

	while let Some(field) = multipart.next_field().await? {
		match field.name() {
			Some("cosmetic_id") => {
				cosmetic_id = field.text().await?.trim().parse::<i32>().ok();
			}
			Some("file") => {
				if let Some(file_name) = field.file_name()
					&& let Some(ext) = std::path::Path::new(file_name).extension()
				{
					extension = ext.to_string_lossy().to_string();
				}
				content_type = field.content_type().map(|s| s.to_string());
				file_data = Some(field.bytes().await?);
			}
			_ => {}
		}
	}

	let cosmetic_id = cosmetic_id.ok_or(ReplaceAssetError::InvalidId)?;
	let data = file_data.ok_or(UploadError::MissingFile)?;

	let Some(cosmetic) = Cosmetic::find_by_id(cosmetic_id)
		.one(&state.database)
		.await?
	else {
		return Err(ReplaceAssetError::MissingCosmetic);
	};

	let slots = super::render_cover::cosmetic_slots(&state.database, &cosmetic).await?;
	let (asset, cover_asset_id) = store_cosmetic_file(
		&state,
		&cosmetic.r#type,
		&slots,
		cosmetic.model_variant.as_deref(),
		&data,
		extension,
		content_type,
	)
	.await?;

	// A failed render keeps the old cover rather than none.
	let cover_asset_id = cover_asset_id.or(cosmetic.cover_asset_id);
	let mut active: cosmetic::ActiveModel = cosmetic.into();
	active.asset_id = Set(Some(asset.id));
	active.cover_asset_id = Set(cover_asset_id);
	let updated = active.update(&state.database).await?;

	Ok(Json(ReplaceAssetResponse {
		cosmetic_id: updated.id,
		asset_id: asset.id,
		cover_asset_id,
	}))
}
