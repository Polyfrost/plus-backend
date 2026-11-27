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
use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait,
	sea_query::Expr,
};
use serde::Serialize;

use super::create::{UploadError, automatic_nameplate_cover, store_cosmetic_file};
use crate::api::{
	ApiState,
	permissions::{Require, perm},
	v0::cosmetics::{
		engine::BundleAnalysis, limits::REPLACE_ASSET_BODY_LIMIT, nameplate::EngineIssue,
	},
};

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
struct ReplaceAssetResponse {
	cosmetic_id: i32,
	asset_id: i32,
	cover_asset_id: Option<i32>,
	preview_asset_id: Option<i32>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	warnings: Vec<EngineIssue>,
	#[serde(skip_serializing_if = "Option::is_none")]
	analysis: Option<BundleAnalysis>,
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
			res.description("Missing or invalid credentials, or lacking the permission")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/replace-asset",
			post_with(self::endpoint, self::endpoint_doc),
		)
		.layer(DefaultBodyLimit::max(REPLACE_ASSET_BODY_LIMIT))
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
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
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

	let shared = match (&cosmetic.r#type, cosmetic.asset_id) {
		(CosmeticType::Nameplate, Some(asset_id)) => {
			Cosmetic::find()
				.filter(cosmetic::Column::AssetId.eq(asset_id))
				.filter(cosmetic::Column::Id.ne(cosmetic.id))
				.all(&state.database)
				.await?
		}
		_ => Vec::new(),
	};
	let variant_keys: Vec<Option<&str>> = std::iter::once(&cosmetic)
		.chain(&shared)
		.map(|row| row.variant_key.as_deref())
		.collect();

	let slots = super::render_cover::cosmetic_slots(&state.database, &cosmetic).await?;
	let stored = store_cosmetic_file(
		&state,
		&cosmetic.r#type,
		&slots,
		cosmetic.model_variant.as_deref(),
		None,
		data,
		extension,
		content_type,
		&variant_keys,
	)
	.await?;
	let asset = stored.asset;

	let mut covers = Vec::new();
	if let Some(bundle) = &stored.nameplate {
		for row in std::iter::once(&cosmetic).chain(&shared) {
			if let Some(cover) =
				automatic_nameplate_cover(&state, bundle.clone(), row.variant_key.clone())
					.await
			{
				covers.push((row.id, cover));
			}
		}
	}
	let own_cover = covers
		.iter()
		.find(|(id, _)| *id == cosmetic.id)
		.map(|(_, cover)| *cover);

	// A failed render keeps the old cover and preview rather than none.
	let (cover_asset_id, preview_asset_id) = next_renders(
		stored.renders,
		own_cover,
		stored.render_skipped,
		cosmetic.cover_asset_id,
		cosmetic.preview_asset_id,
	);
	let cosmetic_id = cosmetic.id;
	let shared_asset_id = match cosmetic.r#type {
		CosmeticType::Nameplate => cosmetic.asset_id,
		_ => None,
	};
	let mut active: cosmetic::ActiveModel = cosmetic.into();
	active.asset_id = Set(Some(asset.id));
	active.cover_asset_id = Set(cover_asset_id);
	active.preview_asset_id = Set(preview_asset_id);

	let txn = state.database.begin().await?;
	let updated = active.update(&txn).await?;
	if let Some(old_asset_id) = shared_asset_id {
		Cosmetic::update_many()
			.col_expr(cosmetic::Column::AssetId, Expr::value(asset.id))
			.filter(cosmetic::Column::AssetId.eq(old_asset_id))
			.filter(cosmetic::Column::Type.eq(CosmeticType::Nameplate))
			.exec(&txn)
			.await?;
	}
	for (id, cover) in covers.into_iter().filter(|(id, _)| *id != cosmetic_id) {
		Cosmetic::update_many()
			.col_expr(cosmetic::Column::CoverAssetId, Expr::value(cover))
			.filter(cosmetic::Column::Id.eq(id))
			.exec(&txn)
			.await?;
	}
	txn.commit().await?;

	crate::api::v0::cosmetics::notify_catalog_updated(&state);

	Ok(Json(ReplaceAssetResponse {
		cosmetic_id: updated.id,
		asset_id: asset.id,
		cover_asset_id,
		preview_asset_id,
		warnings: stored.warnings,
		analysis: stored.analysis,
	}))
}

fn next_renders(
	rendered: Option<(i32, Option<i32>)>,
	own_cover: Option<i32>,
	render_skipped: bool,
	cover: Option<i32>,
	preview: Option<i32>,
) -> (Option<i32>, Option<i32>) {
	let kept_preview = if render_skipped { None } else { preview };
	match (rendered, own_cover) {
		(Some((cover, preview)), _) => (Some(cover), preview),
		(None, Some(cover)) => (Some(cover), kept_preview),
		(None, None) => (cover, kept_preview),
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::{ReplaceAssetResponse, next_renders};
	use crate::api::v0::cosmetics::nameplate::{EngineIssue, IssueLevel};

	#[test]
	fn the_response_adds_warnings_only_when_present() {
		let response = |warnings| {
			let response = ReplaceAssetResponse {
				cosmetic_id: 812,
				asset_id: 40,
				cover_asset_id: Some(41),
				preview_asset_id: None,
				warnings,
				analysis: None,
			};
			serde_json::to_value(response).unwrap()
		};
		let plain = json!({ "cosmetic_id": 812, "asset_id": 40, "cover_asset_id": 41, "preview_asset_id": null });
		assert_eq!(response(Vec::new()), plain);
		let warning = EngineIssue {
			code: "badge_hidden".to_string(),
			level: IssueLevel::Warning,
			path: "manifest.json#badge".to_string(),
			values: [("covered".to_string(), 0.75)].into(),
			names: [("layer".to_string(), "frame".to_string())].into(),
		};
		let mut warned = plain;
		warned["warnings"] = json!([{
			"code": "badge_hidden",
			"level": "warning",
			"path": "manifest.json#badge",
			"values": { "covered": 0.75 },
			"names": { "layer": "frame" }
		}]);
		assert_eq!(response(vec![warning]), warned);
	}

	#[test]
	fn renders_replace_the_cover_and_preview_unless_they_failed() {
		assert_eq!(next_renders(None, None, true, Some(3), Some(4)), (Some(3), None));
		assert_eq!(next_renders(None, None, false, Some(3), Some(4)), (Some(3), Some(4)));
		assert_eq!(next_renders(Some((8, None)), None, false, Some(3), Some(4)), (Some(8), None));
		assert_eq!(next_renders(Some((8, Some(9))), None, false, Some(3), Some(4)), (Some(8), Some(9)));
	}
}
