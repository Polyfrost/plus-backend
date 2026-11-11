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
use std::collections::HashMap;

use entities::sea_orm_active_enums::{AssetKind, BodySlot, CosmeticType};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		admin_auth::AdminAuthenticationExtractor,
		v0::cosmetics::{
			CosmeticInfo, group_cosmetics,
			nameplate::{
				NameplateBundle, NameplateBundleError, StoredBundleError, stored_bundle,
			},
		},
	},
	paynow::{PayNowError, catalog},
	utils::{
		hash::sha256_hex,
		money::to_cents,
		zip::{is_zip, strip_macos_junk, strip_macos_junk_capped},
	},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum UploadError {
	#[error("Missing file in multipart data")]
	MissingFile,
	#[error("Missing or invalid cosmetic type")]
	InvalidType,
	#[error("At least one body slot must be provided")]
	MissingSlots,
	#[error("Invalid body slot")]
	InvalidSlot,
	#[error(transparent)]
	Description(#[from] crate::paynow::catalog::DescriptionLength),
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
	#[error("Invalid ZIP bundle: {0}")]
	Zip(#[from] zip::result::ZipError),
	#[error(transparent)]
	Nameplate(#[from] NameplateBundleError),
	#[error("Nameplates take exactly one body slot, `nameplate`")]
	InvalidNameplateSlots,
	#[error("Send either a file or asset_from, not both")]
	FileAndAssetFrom,
	#[error("asset_from must name another nameplate that has an asset")]
	InvalidAssetFrom,
	#[error("The cover must be a PNG image")]
	InvalidCover,
	#[error("Unable to store the cover: {0}")]
	Cover(#[from] crate::api::v0::collections::StoreAssetError),
}

impl From<StoredBundleError> for UploadError {
	fn from(error: StoredBundleError) -> Self {
		match error {
			StoredBundleError::Bundle(error) => Self::Nameplate(error),
			StoredBundleError::S3(error) => Self::S3(error),
		}
	}
}

impl IntoResponse for UploadError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::Description(_) => StatusCode::BAD_REQUEST,
				Self::MissingFile
				| Self::InvalidType
				| Self::MissingSlots
				| Self::InvalidSlot
				| Self::Zip(_)
				| Self::Nameplate(_)
				| Self::InvalidNameplateSlots
				| Self::FileAndAssetFrom
				| Self::InvalidAssetFrom
				| Self::InvalidCover
				| Self::Rejection(_) => StatusCode::BAD_REQUEST,
				Self::PayNow(_) => StatusCode::BAD_GATEWAY,
				Self::Database(_) | Self::S3(_) | Self::Multipart(_) | Self::Cover(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self,
		)
	}
}

fn parse_cosmetic_type(value: &str) -> Option<CosmeticType> {
	match value {
		"cape" => Some(CosmeticType::Cape),
		"backpack" => Some(CosmeticType::Backpack),
		"glasses" => Some(CosmeticType::Glasses),
		"wings" => Some(CosmeticType::Wings),
		"glove" => Some(CosmeticType::Glove),
		"hat" => Some(CosmeticType::Hat),
		"aura" => Some(CosmeticType::Aura),
		"boots" => Some(CosmeticType::Boots),
		"shoulder" => Some(CosmeticType::Shoulder),
		"pet" => Some(CosmeticType::Pet),
		"nameplate" => Some(CosmeticType::Nameplate),
		"emote" => Some(CosmeticType::Emote),
		_ => None,
	}
}

fn parse_body_slot(value: &str) -> Option<BodySlot> {
	match value {
		"cape" => Some(BodySlot::Cape),
		"backpack" => Some(BodySlot::Backpack),
		"glasses" => Some(BodySlot::Glasses),
		"wings" => Some(BodySlot::Wings),
		"left_hand" => Some(BodySlot::LeftHand),
		"right_hand" => Some(BodySlot::RightHand),
		"hat" => Some(BodySlot::Hat),
		"aura" => Some(BodySlot::Aura),
		"boots" => Some(BodySlot::Boots),
		"shoulder" => Some(BodySlot::Shoulder),
		"pet" => Some(BodySlot::Pet),
		"nameplate" => Some(BodySlot::Nameplate),
		_ => None,
	}
}

fn default_name(cosmetic_type: &CosmeticType) -> &'static str {
	match cosmetic_type {
		CosmeticType::Cape => "Cape",
		CosmeticType::Backpack => "Backpack",
		CosmeticType::Glasses => "Glasses",
		CosmeticType::Wings => "Wings",
		CosmeticType::Glove => "Glove",
		CosmeticType::Hat => "Hat",
		CosmeticType::Aura => "Aura",
		CosmeticType::Boots => "Boots",
		CosmeticType::Shoulder => "Shoulder",
		CosmeticType::Pet => "Pet",
		CosmeticType::Nameplate => "Nameplate",
		CosmeticType::Emote => "Emote",
	}
}

fn storage_prefix(cosmetic_type: &CosmeticType) -> &'static str {
	match cosmetic_type {
		CosmeticType::Cape => "capes",
		CosmeticType::Backpack => "backpacks",
		CosmeticType::Glasses => "glasses",
		CosmeticType::Wings => "wings",
		CosmeticType::Glove => "gloves",
		CosmeticType::Hat => "hats",
		CosmeticType::Aura => "auras",
		CosmeticType::Boots => "boots",
		CosmeticType::Shoulder => "shoulders",
		CosmeticType::Pet => "pets",
		CosmeticType::Nameplate => "nameplates",
		CosmeticType::Emote => "emotes",
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("createCosmetic")
		.summary("Create a new cosmetic")
		.description(
			"Uploads a new cosmetic to S3 and registers it in the database with its \
			 allowed body slots. It is only sold once it has a product: pass \
			 `base_price` to provision one now, or leave it out and price it later \
			 (or sell it in a bundle). A cosmetic joining a group that already has a \
			 product joins that product. Emotes (type `emote`) are stored as bundles \
			 and take no body slots.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::OK.as_u16() }, Json<CosmeticInfo>, _>(|res| {
			res.description("The created cosmetic info")
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
struct CosmeticUploadRequest {
	#[schemars(with = "String")]
	file: String,
	/// The cosmetic type: one of `cape`, `backpack`, `glasses`, `wings`, `glove`,
	/// `hat`, `aura`, `boots`, `shoulder`, `emote`.
	r#type: String,
	/// Optional display name; defaults to the type name.
	name: Option<String>,
	/// Optional long-form description for the catalog and storefront product.
	description: Option<String>,
	/// Optional id of the collection this cosmetic belongs to.
	collection: Option<i32>,
	/// The price in USD major units (e.g. `4.99`). Leave out to upload without
	/// making it a product; ignored when joining a group that has one.
	base_price: Option<f32>,
	/// One or more allowed body slots (repeat the field for multiple).
	/// not required for emotes
	slots: Vec<String>,
	/// Optional group name. When set, this cosmetic becomes a variant of the
	/// (find-or-created) group of the same name and type, so the player buys
	/// the group once and chooses between its variants.
	group: Option<String>,
	/// Optional user-facing variant label within the group (e.g. "Blue").
	variant_name: Option<String>,
	/// Optional skin model this variant targets ("slim"/"wide"); only for
	/// variants the client must pick by skin.
	model_variant: Option<String>,
	/// Optional ordering of this variant within its group (default 0).
	variant_order: Option<i32>,
	variant_key: Option<String>,
	asset_from: Option<i32>,
	#[schemars(with = "Option<String>")]
	cover: Option<String>,
}

impl OperationInput for FileUpload {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.request_body = Some(aide::openapi::ReferenceOr::Item(
			aide::openapi::RequestBody {
				description: Some("Multipart cosmetic upload".into()),
				content: [(
					"multipart/form-data".into(),
					aide::openapi::MediaType {
						schema: Some(aide::openapi::SchemaObject {
							json_schema: ctx
								.schema
								.subschema_for::<CosmeticUploadRequest>(),
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
	let mut cosmetic_type = None;
	let mut name = None;
	let mut description = None;
	let mut collection = None;
	let mut base_price = None;
	let mut slots = Vec::new();
	let mut group_name = None;
	let mut variant_name = None;
	let mut model_variant = None;
	let mut variant_order = 0;
	let mut variant_key = None;
	let mut asset_from = None;
	let mut cover_data = None;

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
			Some("type") => {
				let value = field.text().await?;
				cosmetic_type = Some(
					parse_cosmetic_type(value.trim()).ok_or(UploadError::InvalidType)?,
				);
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
					crate::paynow::catalog::check_description(Some(trimmed))?;
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
			Some("slots") => {
				let value = field.text().await?;
				slots
					.push(parse_body_slot(value.trim()).ok_or(UploadError::InvalidSlot)?);
			}
			Some("group") => {
				let value = field.text().await?;
				let trimmed = value.trim();
				if !trimmed.is_empty() {
					group_name = Some(trimmed.to_string());
				}
			}
			Some("variant_name") => {
				let value = field.text().await?;
				let trimmed = value.trim();
				if !trimmed.is_empty() {
					variant_name = Some(trimmed.to_string());
				}
			}
			Some("model_variant") => {
				let value = field.text().await?;
				let trimmed = value.trim();
				if !trimmed.is_empty() {
					model_variant = Some(trimmed.to_string());
				}
			}
			Some("asset_from") => {
				let value = field.text().await?;
				let trimmed = value.trim();
				if !trimmed.is_empty() {
					asset_from = Some(
						trimmed
							.parse::<i32>()
							.map_err(|_| UploadError::InvalidAssetFrom)?,
					);
				}
			}
			Some("cover") => {
				cover_data = Some(field.bytes().await?);
			}
			Some("variant_key") => {
				let value = field.text().await?;
				let trimmed = value.trim();
				if !trimmed.is_empty() {
					variant_key = Some(trimmed.to_string());
				}
			}
			Some("variant_order") => {
				let value = field.text().await?;
				if let Ok(parsed) = value.trim().parse::<i32>() {
					variant_order = parsed;
				}
			}
			_ => {}
		}
	}

	let cosmetic_type = cosmetic_type.ok_or(UploadError::InvalidType)?;

	let is_emote = matches!(cosmetic_type, CosmeticType::Emote);
	let is_nameplate = matches!(cosmetic_type, CosmeticType::Nameplate);
	if !is_emote && slots.is_empty() {
		return Err(UploadError::MissingSlots);
	}
	if is_nameplate && slots != [BodySlot::Nameplate] {
		return Err(UploadError::InvalidNameplateSlots);
	}
	if cover_data.as_deref().is_some_and(|cover| !is_png(cover)) {
		return Err(UploadError::InvalidCover);
	}

	use entities::{
		cosmetic, cosmetic_allowed_slot, cosmetic_group, cosmetic_group_allowed_slot,
		prelude::*,
	};

	let (asset, rendered_cover_id) = match (file_data, asset_from) {
		(Some(_), Some(_)) => return Err(UploadError::FileAndAssetFrom),
		(None, None) => return Err(UploadError::MissingFile),
		(Some(data), None) => {
			store_cosmetic_file(
				&state,
				&cosmetic_type,
				&slots,
				model_variant.as_deref(),
				&data,
				extension,
				content_type,
				&[variant_key.as_deref()],
			)
			.await?
		}
		(None, Some(source_id)) => {
			if !is_nameplate {
				return Err(UploadError::InvalidAssetFrom);
			}
			let source = Cosmetic::find_by_id(source_id)
				.one(&state.database)
				.await?
				.filter(|source| source.r#type == CosmeticType::Nameplate)
				.ok_or(UploadError::InvalidAssetFrom)?;
			let asset = match source.asset_id {
				Some(asset_id) => {
					Asset::find_by_id(asset_id).one(&state.database).await?
				}
				None => None,
			}
			.ok_or(UploadError::InvalidAssetFrom)?;
			if let Some(bundle) = stored_bundle(&state, &asset).await? {
				bundle.check_variant_key(variant_key.as_deref())?;
			}
			(asset, None)
		}
	};

	let cover_asset_id = match cover_data {
		Some(cover) => Some(store_cover(&state, &cover).await?),
		None => rendered_cover_id,
	};

	let group = match group_name {
		Some(group_name) => {
			let existing = CosmeticGroup::find()
				.filter(cosmetic_group::Column::Name.eq(group_name.clone()))
				.filter(cosmetic_group::Column::Type.eq(cosmetic_type.clone()))
				.one(&state.database)
				.await?;
			let group = match existing {
				Some(group) => group,
				None => {
					cosmetic_group::ActiveModel {
						name: Set(group_name),
						r#type: Set(cosmetic_type.clone()),
						enabled: Set(true),
						..Default::default()
					}
					.insert(&state.database)
					.await?
				}
			};
			if !slots.is_empty() {
				cosmetic_group_allowed_slot::Entity::insert_many(slots.iter().map(
					|slot| cosmetic_group_allowed_slot::ActiveModel {
						group_id: Set(group.id),
						slot: Set(slot.clone()),
					},
				))
				.on_conflict_do_nothing()
				.exec(&state.database)
				.await?;
			}
			Some(group)
		}
		None => None,
	};

	let resolved_name = name.unwrap_or_else(|| default_name(&cosmetic_type).to_string());

	// Variants share one product, so a new one reuses a sibling's ids.
	let sibling = match &group {
		Some(group) => {
			Cosmetic::find()
				.filter(cosmetic::Column::GroupId.eq(group.id))
				.filter(cosmetic::Column::StoreProductId.is_not_null())
				.one(&state.database)
				.await?
		}
		None => None,
	};

	let (store_product_id, price_value) = match sibling {
		Some(sibling) => (sibling.store_product_id, sibling.base_price),
		// No price: upload only, priced later or bundled.
		None => (None, base_price),
	};

	let model = cosmetic::ActiveModel {
		asset_id: Set(Some(asset.id)),
		cover_asset_id: Set(cover_asset_id),
		name: Set(Some(resolved_name)),
		r#type: Set(cosmetic_type),
		enabled: Set(true),
		group_id: Set(group.as_ref().map(|g| g.id)),
		variant_name: Set(variant_name),
		model_variant: Set(model_variant),
		variant_order: Set(variant_order),
		variant_key: Set(variant_key),
		store_product_id: Set(store_product_id),
		base_price: Set(price_value),
		collection: Set(collection),
		description: Set(description),
		..Default::default()
	}
	.insert(&state.database)
	.await?;

	// A failure here leaves a priced cosmetic with no product, which
	// `provision-paynow` picks up on its next run.
	let model = match (&model.store_product_id, price_value) {
		(None, Some(base_price)) => {
			let slug = match group.as_ref() {
				Some(group) => catalog::cosmetic_group_slug(group.id),
				None => catalog::cosmetic_slug(model.id),
			};
			let product_id = state
				.paynow
				.client
				.create_product(
					&slug,
					group
						.as_ref()
						.map(|group| group.name.as_str())
						.or(model.name.as_deref())
						.unwrap_or(&slug),
					model.description.as_deref(),
					to_cents(base_price),
					!model.enabled,
				)
				.await?;

			let mut active: cosmetic::ActiveModel = model.into();
			active.store_product_id = Set(Some(product_id));
			active.update(&state.database).await?
		}
		_ => model,
	};

	if !slots.is_empty() {
		cosmetic_allowed_slot::Entity::insert_many(slots.iter().map(|slot| {
			cosmetic_allowed_slot::ActiveModel {
				cosmetic_id: Set(model.id),
				slot: Set(slot.clone()),
			}
		}))
		.exec(&state.database)
		.await?;
	}

	let groups = match &group {
		Some(group) => {
			let group_slots = CosmeticGroupAllowedSlot::find()
				.filter(cosmetic_group_allowed_slot::Column::GroupId.eq(group.id))
				.all(&state.database)
				.await?
				.into_iter()
				.map(|s| s.slot)
				.collect();
			HashMap::from([(group.id, (group.clone(), group_slots))])
		}
		None => HashMap::new(),
	};

	let cover_asset = match cover_asset_id {
		Some(cover_asset_id) => {
			Asset::find_by_id(cover_asset_id)
				.one(&state.database)
				.await?
		}
		None => None,
	};

	let mut infos = group_cosmetics(
		vec![(model, Some(asset), cover_asset, slots)],
		groups,
		state.asset_cache.clone(),
		state.s3_bucket.clone(),
		&state.s3_public_url,
		false,
	)
	.await?;

	crate::api::v0::cosmetics::notify_catalog_updated(&state).await;

	Ok(Json(infos.remove(0)))
}

/// Uploads a cosmetic's source file, renders its cover when the render service
/// is configured, and returns the new asset with the cover's id.
#[allow(clippy::too_many_arguments)]
pub(super) async fn store_cosmetic_file(
	state: &ApiState,
	cosmetic_type: &CosmeticType,
	slots: &[BodySlot],
	model_variant: Option<&str>,
	data: &[u8],
	mut extension: String,
	mut content_type: Option<String>,
	variant_keys: &[Option<&str>],
) -> Result<(entities::asset::Model, Option<i32>), UploadError> {
	use entities::asset;

	let is_emote = matches!(cosmetic_type, CosmeticType::Emote);
	let is_bundle = is_zip(data);
	let data: Vec<u8> = if is_bundle && matches!(cosmetic_type, CosmeticType::Nameplate) {
		strip_macos_junk_capped(
			data,
			crate::api::v0::cosmetics::nameplate::MAX_UNPACKED_BYTES,
		)?
	} else if is_bundle {
		strip_macos_junk(data)?
	} else {
		data.to_vec()
	};
	if matches!(cosmetic_type, CosmeticType::Nameplate) {
		let bundle = NameplateBundle::read(&data)?;
		for key in variant_keys {
			bundle.check_variant_key(*key)?;
		}
	}
	if is_bundle {
		extension = "zip".to_string();
		content_type = Some("application/zip".to_string());
	}
	let asset_kind = if is_emote || is_bundle {
		AssetKind::Bundle
	} else {
		AssetKind::Image
	};
	let default_content_type = if is_emote {
		"application/zip"
	} else {
		"image/png"
	};

	let path = format!(
		"{}/{}.{}",
		storage_prefix(cosmetic_type),
		Uuid::now_v7(),
		extension
	);

	state
		.s3_bucket
		.put_object_with_content_type(
			&path,
			&data,
			content_type.as_deref().unwrap_or(default_content_type),
		)
		.await?;

	let asset = asset::ActiveModel {
		storage_path: Set(Some(path)),
		url: Set(None),
		asset_kind: Set(asset_kind),
		content_type: Set(content_type.or_else(|| Some(default_content_type.to_string()))),
		hash: Set(Some(sha256_hex(&data))),
		..Default::default()
	}
	.insert(&state.database)
	.await?;

	let cover_asset_id = if state.render_service_url.is_empty()
		|| matches!(cosmetic_type, CosmeticType::Nameplate)
	{
		None
	} else {
		match crate::api::v0::cosmetics::cover::render_cover(
			&state.render_client,
			&state.render_service_url,
			cosmetic_type,
			slots,
			model_variant,
			is_bundle,
			&data,
		)
		.await
		{
			Ok(png) => match crate::api::v0::collections::store_asset(
				state,
				&png,
				Some("image/png".to_string()),
				"png",
				"covers",
			)
			.await
			{
				Ok(id) => Some(id),
				Err(error) => {
					tracing::warn!("Unable to store cosmetic cover asset: {error}");
					None
				}
			},
			Err(error) => {
				tracing::warn!("Unable to render cosmetic cover: {error}");
				None
			}
		}
	};

	let info = crate::api::v0::cosmetics::CachedAssetInfo::from_db_model(
		&asset,
		state.s3_bucket.clone(),
	)
	.await?;
	state.asset_cache.insert(asset.id, info).await;

	Ok((asset, cover_asset_id))
}

fn is_png(data: &[u8]) -> bool {
	data.starts_with(b"\x89PNG\r\n\x1a\n")
}

pub(super) async fn store_cover(
	state: &ApiState,
	data: &[u8],
) -> Result<i32, UploadError> {
	if !is_png(data) {
		return Err(UploadError::InvalidCover);
	}
	Ok(crate::api::v0::collections::store_asset(
		state,
		data,
		Some("image/png".to_string()),
		"png",
		"covers",
	)
	.await?)
}
