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
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use uuid::Uuid;

use crate::{
	api::{ApiState, admin_auth::AdminAuthenticationExtractor, v0::bundles::BundleInfo},
	paynow::{PayNowError, catalog},
	utils::{hash::sha256_hex, money::to_cents},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum CreateError {
	#[error("A bundle name is required")]
	MissingName,
	#[error("A base price is required to create a new storefront product")]
	MissingPrice,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("S3 error: {0}")]
	S3(#[from] s3::error::S3Error),
	#[error("PayNow error: {0}")]
	PayNow(#[from] PayNowError),
	#[error("Multipart error: {0}")]
	Multipart(#[from] axum::extract::multipart::MultipartError),
	#[error("Multipart rejection: {0}")]
	Rejection(#[from] MultipartRejection),
}

impl IntoResponse for CreateError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::MissingName | Self::MissingPrice | Self::Rejection(_) => {
					StatusCode::BAD_REQUEST
				}
				Self::PayNow(_) => StatusCode::BAD_GATEWAY,
				Self::Database(_) | Self::S3(_) | Self::Multipart(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self,
		)
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("createBundle")
		.summary("Create a new bundle")
		.description(
			"Uploads a bundle's cover image to S3 (optional), registers the bundle \
			 in the database with its contained cosmetics, then provisions a \
			 storefront product for it. Admin password required.",
		)
		.tag("bundles")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<BundleInfo>, _>(|res| {
			res.description("The created bundle info")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/create", post_with(self::endpoint, self::endpoint_doc))
}

struct FileUpload(Multipart);

impl<S> FromRequest<S> for FileUpload
where
	S: Send + Sync,
{
	type Rejection = CreateError;

	async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
		Ok(Self(
			Multipart::from_request(req, state)
				.await
				.map_err(CreateError::Rejection)?,
		))
	}
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct BundleUploadRequest {
	/// Optional cover image for the bundle.
	#[schemars(with = "Option<String>")]
	file: Option<String>,
	/// The bundle's display name.
	name: String,
	/// Optional long-form description for the catalog and Stripe product.
	description: Option<String>,
	/// Optional id of the collection this bundle belongs to.
	collection: Option<i32>,
	/// The price in USD major units (e.g. `9.99`). Required to create the Stripe
	/// product and price.
	base_price: Option<f32>,
	/// The ids of the cosmetics (and emotes) this bundle contains (repeat the
	/// field for multiple).
	cosmetic_id: Vec<i32>,
}

impl OperationInput for FileUpload {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.request_body = Some(aide::openapi::ReferenceOr::Item(
			aide::openapi::RequestBody {
				description: Some("Multipart bundle upload".into()),
				content: [(
					"multipart/form-data".into(),
					aide::openapi::MediaType {
						schema: Some(aide::openapi::SchemaObject {
							json_schema: ctx
								.schema
								.subschema_for::<BundleUploadRequest>(),
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
) -> Result<Json<BundleInfo>, CreateError> {
	let mut file_data = None;
	let mut content_type = None;
	let mut extension = "png".to_string();
	let mut name = None;
	let mut description = None;
	let mut collection = None;
	let mut base_price = None;
	let mut cosmetic_ids = Vec::new();

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
			Some("description") => {
				let value = field.text().await?;
				let trimmed = value.trim();
				if !trimmed.is_empty() {
					description = Some(trimmed.to_string());
				}
			}
			Some("collection") => {
				let value = field.text().await?;
				if let Ok(parsed) = value.trim().parse::<i32>() {
					collection = Some(parsed);
				}
			}
			Some("base_price") => {
				let value = field.text().await?;
				if let Ok(parsed) = value.trim().parse::<f32>() {
					base_price = Some(parsed);
				}
			}
			Some("cosmetic_id") => {
				let value = field.text().await?;
				if let Ok(parsed) = value.trim().parse::<i32>() {
					cosmetic_ids.push(parsed);
				}
			}
			_ => {}
		}
	}

	let name = name.ok_or(CreateError::MissingName)?;

	// Upload the cover image to S3 when one was provided.
	let asset_id = match file_data {
		Some(data) => {
			let path = format!("bundles/{}.{}", Uuid::now_v7(), extension);
			state
				.s3_bucket
				.put_object_with_content_type(
					&path,
					&data,
					content_type.as_deref().unwrap_or("image/png"),
				)
				.await?;

			use entities::asset;
			let asset = asset::ActiveModel {
				storage_path: Set(Some(path)),
				url: Set(None),
				asset_kind: Set(AssetKind::Image),
				content_type: Set(content_type.or_else(|| Some("image/png".to_string()))),
				hash: Set(Some(sha256_hex(&data))),
				..Default::default()
			}
			.insert(&state.database)
			.await?;
			Some(asset.id)
		}
		None => None,
	};

	let base_price = base_price.ok_or(CreateError::MissingPrice)?;

	use entities::{bundles, bundles_cosmetics, prelude::*};

	// Inserted first because the slug needs the row id. A failure to provision
	// leaves a priced bundle that `provision-paynow` picks up.
	let bundle = bundles::ActiveModel {
		name: Set(name),
		description: Set(description),
		asset_id: Set(asset_id),
		enabled: Set(true),
		collection: Set(collection),
		store_product_id: Set(None),
		base_price: Set(Some(base_price)),
		discount_rate: Set(None),
		..Default::default()
	}
	.insert(&state.database)
	.await?;

	let product_id = state
		.paynow
		.client
		.create_product(
			&catalog::bundle_slug(bundle.id),
			&bundle.name,
			bundle.description.as_deref(),
			to_cents(base_price),
			false,
		)
		.await?;

	let mut active: bundles::ActiveModel = bundle.into();
	active.store_product_id = Set(Some(product_id));
	let bundle = active.update(&state.database).await?;

	if !cosmetic_ids.is_empty() {
		BundlesCosmetics::insert_many(cosmetic_ids.iter().map(|cosmetic_id| {
			bundles_cosmetics::ActiveModel {
				bundle_id: Set(bundle.id),
				cosmetic_id: Set(*cosmetic_id),
			}
		}))
		.on_conflict_do_nothing()
		.exec(&state.database)
		.await?;
	}

	Ok(Json(bundle.into()))
}
