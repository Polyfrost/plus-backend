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
use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ActiveValue, Set};
use uuid::Uuid;

use crate::api::{
	ApiState, admin_auth::AdminAuthenticationExtractor, cosmetics::CosmeticInfo,
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
}

impl IntoResponse for UploadError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::MissingFile | Self::Rejection(_) => StatusCode::BAD_REQUEST,
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
	op.id("uploadCape")
		.summary("Upload a new cape")
		.description(
			"Uploads a new cape cosmetic to S3 and registers it in the database.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<CosmeticInfo>, _>(|res| {
			res.description("The uploaded cosmetic info")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/cape", post_with(self::endpoint, self::endpoint_doc))
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
struct CapeUploadRequest {
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
				description: Some("Multipart file upload".into()),
				content: [(
					"multipart/form-data".into(),
					aide::openapi::MediaType {
						schema: Some(aide::openapi::SchemaObject {
							json_schema: ctx.schema.subschema_for::<CapeUploadRequest>(),
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
) -> Result<Json<CosmeticInfo>, UploadError> {
	let mut file_data = None;
	let mut content_type = None;
	let mut extension = "png".to_string();

	while let Some(field) = multipart.next_field().await? {
		if field.name() == Some("file") {
			if let Some(name) = field.file_name()
				&& let Some(ext) = std::path::Path::new(name).extension()
			{
				extension = ext.to_string_lossy().to_string();
			}
			content_type = field.content_type().map(|s| s.to_string());
			file_data = Some(field.bytes().await?);
			break;
		}
	}

	let Some(data) = file_data else {
		return Err(UploadError::MissingFile);
	};

	let path = format!("capes/{}.{}", Uuid::now_v7(), extension);

	state
		.s3_bucket
		.put_object_with_content_type(
			&path,
			&data,
			content_type.as_deref().unwrap_or("image/png"),
		)
		.await?;

	use entities::cosmetic;

	let model = cosmetic::ActiveModel {
		r#type: Set(CosmeticType::Cape),
		path: Set(Some(path)),
		..Default::default()
	}
	.insert(&state.database)
	.await?;

	let info = crate::api::cosmetics::CachedCosmeticInfo::from_db_model(
		&model,
		state.s3_bucket.clone(),
	)
	.await?;
	state.cosmetic_cache.insert(model.id, info).await;

	Ok(Json(
		CosmeticInfo::from_db_model(
			&model,
			state.cosmetic_cache.clone(),
			state.s3_bucket.clone(),
		)
		.await?,
	))
}
