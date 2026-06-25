use aide::{
	OperationInput, OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{FromRequest, Multipart, Request, State, multipart::MultipartRejection},
	http::StatusCode,
	response::IntoResponse,
};
use entities::sea_orm_active_enums::AssetKind;
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, Set};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::api::{
	ApiState, admin_auth::AdminAuthenticationExtractor, cosmetics::EmoteInfo,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum UploadError {
	#[error("Missing file in multipart data")]
	MissingFile,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("S3 error: {0}")]
	S3(#[from] s3::error::S3Error),
	#[error("Multipart error: {0}")]
	Multipart(#[from] axum::extract::multipart::MultipartError),
	#[error("Multipart rejection: {0}")]
	Rejection(#[from] MultipartRejection),
	#[error("Invalid ZIP bundle: {0}")]
	Zip(#[from] zip::result::ZipError),
}

impl IntoResponse for UploadError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::MissingFile | Self::Zip(_) | Self::Rejection(_) => {
					StatusCode::BAD_REQUEST
				}
				Self::Database(_) | Self::S3(_) | Self::Multipart(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self.to_string(),
		)
			.into_response()
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("uploadEmote")
		.summary("Upload a new emote")
		.description(
			"Uploads a new emote cosmetic archive to S3 and registers it in the database. \
			 ZIP archives are preferred; other file types are stored as a single asset.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<EmoteInfo>, _>(|res| {
			res.description("The uploaded cosmetic info")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/emote", post_with(self::endpoint, self::endpoint_doc))
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
struct EmoteUploadRequest {
	#[schemars(with = "String")]
	file: String,
	name: Option<String>,
}

impl OperationInput for FileUpload {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.request_body = Some(aide::openapi::ReferenceOr::Item(
			aide::openapi::RequestBody {
				description: Some("Multipart file upload".into()),
				content: [(
					"multipart/form-data".into(),
					aide::openapi::MediaType {
						schema: Some(aide::openapi::SchemaObject {
							json_schema: ctx.schema.subschema_for::<EmoteUploadRequest>(),
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

fn default_content_type(extension: &str) -> &'static str {
	match extension {
		"zip" => "application/zip",
		_ => "application/octet-stream",
	}
}

fn sha256_hex(data: &[u8]) -> String {
	Sha256::digest(data)
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect()
}

async fn endpoint(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	FileUpload(mut multipart): FileUpload,
) -> Result<Json<EmoteInfo>, UploadError> {
	let mut file_data = None;
	let mut content_type = None;
	let mut extension = "zip".to_string();
	let mut name = None;

	while let Some(field) = multipart.next_field().await? {
		match field.name() {
			Some("file") => {
				if let Some(file_name) = field.file_name()
					&& let Some(ext) = std::path::Path::new(file_name).extension()
				{
					extension = ext.to_string_lossy().to_string();
				}
				content_type = field.content_type().map(|s| s.to_string());
				file_data = Some(field.bytes().await?);
			}
			Some("name") => {
				let value = field.text().await?;
				let trimmed = value.trim();
				if !trimmed.is_empty() {
					name = Some(trimmed.to_string());
				}
			}
			_ => {}
		}
	}

	let Some(data) = file_data else {
		return Err(UploadError::MissingFile);
	};

	let is_bundle = crate::api::cosmetics::is_zip(&data);
	let data: Vec<u8> = if is_bundle {
		crate::api::cosmetics::strip_macos_junk(&data)?
	} else {
		data.to_vec()
	};
	if is_bundle {
		extension = "zip".to_string();
		content_type = Some("application/zip".to_string());
	}

	let path = format!("emotes/{}.{}", Uuid::now_v7(), extension);

	state
		.s3_bucket
		.put_object_with_content_type(
			&path,
			&data,
			content_type
				.as_deref()
				.unwrap_or_else(|| default_content_type(&extension)),
		)
		.await?;

	use entities::{asset, emote};

	let asset = asset::ActiveModel {
		storage_path: Set(Some(path)),
		url: Set(None),
		asset_kind: Set(AssetKind::Bundle),
		content_type: Set(
			content_type.or_else(|| Some(default_content_type(&extension).to_string()))
		),
		hash: Set(Some(sha256_hex(&data))),
		..Default::default()
	}
	.insert(&state.database)
	.await?;

	let model = emote::ActiveModel {
		asset_id: Set(Some(asset.id)),
		name: Set(name.unwrap_or_else(|| "Emote".to_string())),
		enabled: Set(true),
		..Default::default()
	}
	.insert(&state.database)
	.await?;

	let info = crate::api::cosmetics::CachedAssetInfo::from_db_model(
		&asset,
		state.s3_bucket.clone(),
	)
	.await?;
	state.asset_cache.insert(asset.id, info).await;

	Ok(Json(
		EmoteInfo::from_db_model(
			&model,
			Some(&asset),
			state.asset_cache.clone(),
			state.s3_bucket.clone(),
		)
		.await?,
	))
}
