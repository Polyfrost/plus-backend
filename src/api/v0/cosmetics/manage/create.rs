use aide::{
	OperationInput, OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	body::Bytes,
	extract::{
		DefaultBodyLimit, FromRequest, Multipart, Request, State,
		multipart::MultipartRejection,
	},
	http::StatusCode,
	response::IntoResponse,
};
use std::{collections::HashMap, io::Cursor};

use entities::sea_orm_active_enums::{AssetKind, BodySlot, CosmeticType};
use image::{ImageFormat, ImageReader};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set,
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		permissions::{Require, perm},
		v0::{
			bonuses::sets_changed,
			collections::{StoreAssetError, store_asset},
			cosmetics::{
				CosmeticInfo,
				color_slots::{customizable_slots, store_slots},
				cover::{
					PREVIEW_CONTENT_TYPE, PREVIEW_EXTENSION, RenderError, Renders,
					render_nameplate, renders_cover,
				},
				effect::{EffectSlotError, check_slots},
				engine::{BundleAnalysis, BundleKind, CosmeticBundleError, check_upload},
				limits::{
					BundleLimitError, BundleLimits, CREATE_BODY_LIMIT, MAX_COVER_BYTES,
					check_download, prepare_bundle_off_thread,
				},
				group_cosmetics,
				nameplate::{
					EngineIssue, NameplateBundle, NameplateBundleError,
					StoredBundleError, stored_bundle, validate_upload,
				},
			},
		},
	},
	paynow::{PayNowError, catalog},
	utils::{
		hash::sha256_hex,
		money::to_cents,
		zip::is_zip,
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
	EffectSlots(#[from] EffectSlotError),
	#[error(transparent)]
	BundleLimits(#[from] BundleLimitError),
	#[error("Effect bundles must be zip files")]
	EffectNotZip,
	#[error("Effect cosmetics need a name")]
	MissingName,
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
	#[error(transparent)]
	Bundle(#[from] CosmeticBundleError),
	#[error("Nameplates take exactly one body slot, `nameplate`, and no other type can use it")]
	InvalidNameplateSlots,
	#[error("Send either a file or asset_from, not both")]
	FileAndAssetFrom,
	#[error("asset_from must name another nameplate that has an asset")]
	InvalidAssetFrom,
	#[error("asset_from must name a nameplate in the same group as the new one")]
	AssetFromOtherGroup,
	#[error("Another variant in this group already uses the variant key {0:?}")]
	DuplicateVariantKey(String),
	#[error("enabled must be true or false")]
	InvalidEnabled,
	#[error("The cover must be a valid PNG image")]
	InvalidCover,
	#[error("The cover is {0}x{1} pixels, and covers are at most {MAX_COVER_SIDE}x{MAX_COVER_SIDE}")]
	CoverDimensions(u32, u32),
	#[error("The cover is {0} bytes, and covers are at most {MAX_COVER_BYTES}")]
	CoverTooLarge(usize),
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

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct InvalidBundleBody {
	pub(super) message: String,
	pub(super) issues: Vec<EngineIssue>,
}

impl IntoResponse for UploadError {
	fn into_response(self) -> axum::response::Response {
		if let Self::Nameplate(NameplateBundleError::Invalid(issues))
		| Self::Bundle(CosmeticBundleError::Invalid(issues)) = &self
		{
			let body = InvalidBundleBody {
				message: self.to_string(),
				issues: issues.clone(),
			};
			return (StatusCode::BAD_REQUEST, Json(body)).into_response();
		}
		crate::api::admin_error_response(
			match self {
				Self::Nameplate(NameplateBundleError::ValidatorFailed(_))
				| Self::Bundle(CosmeticBundleError::ValidatorFailed(_))
				| Self::BundleLimits(BundleLimitError::Stopped(_)) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
				Self::Description(_) => StatusCode::BAD_REQUEST,
				Self::MissingFile
				| Self::InvalidType
				| Self::MissingSlots
				| Self::InvalidSlot
				| Self::EffectSlots(_)
				| Self::BundleLimits(_)
				| Self::EffectNotZip
				| Self::MissingName
				| Self::Zip(_)
				| Self::Nameplate(_)
				| Self::Bundle(_)
				| Self::InvalidNameplateSlots
				| Self::FileAndAssetFrom
				| Self::InvalidAssetFrom
				| Self::AssetFromOtherGroup
				| Self::InvalidEnabled
				| Self::InvalidCover
				| Self::CoverDimensions(..)
				| Self::CoverTooLarge(_)
				| Self::Rejection(_) => StatusCode::BAD_REQUEST,
				Self::DuplicateVariantKey(_) => StatusCode::CONFLICT,
				Self::PayNow(_) => StatusCode::BAD_GATEWAY,
				Self::Multipart(ref e) => e.status(),
				Self::Database(_) | Self::S3(_) | Self::Cover(_) => {
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
		"effect" => Some(CosmeticType::Effect),
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
		"effect_a" => Some(BodySlot::EffectA),
		"effect_b" => Some(BodySlot::EffectB),
		"tail" => Some(BodySlot::Tail),
		_ => None,
	}
}

fn default_name(cosmetic_type: &CosmeticType) -> Option<&'static str> {
	Some(match cosmetic_type {
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
		CosmeticType::Effect => return None,
	})
}

fn resolve_name(
	name: Option<String>,
	cosmetic_type: &CosmeticType,
	group_name: Option<&str>,
) -> Result<String, UploadError> {
	name.or_else(|| default_name(cosmetic_type).map(str::to_string))
		.or_else(|| group_name.map(str::to_string))
		.ok_or(UploadError::MissingName)
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
		CosmeticType::Effect => "effects",
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
		.response_with::<{ StatusCode::OK.as_u16() }, Json<CreateResponse>, _>(|res| {
			res.description(
				"The created cosmetic, plus any warnings the bundle check raised",
			)
		})
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, Json<InvalidBundleBody>, _>(
			|res| {
				res.description(concat!(
					"The upload was rejected. A bundle that fails its check gets this ",
					"body; other errors come back as plain text.",
				))
			},
		)
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Missing or invalid credentials, or lacking the permission")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/create", post_with(self::endpoint, self::endpoint_doc))
		.layer(DefaultBodyLimit::max(CREATE_BODY_LIMIT))
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
	/// Optional time in seconds into the emote or cosmetic animation the cover
	/// is captured at (default 1).
	cover_seconds: Option<f32>,
	variant_key: Option<String>,
	asset_from: Option<i32>,
	#[schemars(with = "Option<String>")]
	cover: Option<String>,
	enabled: Option<bool>,
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
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	FileUpload(mut multipart): FileUpload,
) -> Result<Json<CreateResponse>, UploadError> {
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
	let mut cover_seconds = None;
	let mut variant_key = None;
	let mut asset_from = None;
	let mut cover_data = None;
	let mut enabled = None;

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
			Some("cover_seconds") => {
				cover_seconds = field.text().await?.trim().parse::<f32>().ok();
			}
			Some("enabled") => {
				let value = field.text().await?;
				match value.trim().to_ascii_lowercase().as_str() {
					"" => {}
					"true" | "1" => enabled = Some(true),
					"false" | "0" => enabled = Some(false),
					_ => return Err(UploadError::InvalidEnabled),
				}
			}
			_ => {}
		}
	}

	let cosmetic_type = cosmetic_type.ok_or(UploadError::InvalidType)?;
	let listed = listed_on_upload(&cosmetic_type, enabled);

	let is_emote = matches!(cosmetic_type, CosmeticType::Emote);
	let is_nameplate = matches!(cosmetic_type, CosmeticType::Nameplate);
	if !is_emote && slots.is_empty() {
		return Err(UploadError::MissingSlots);
	}
	if !nameplate_slots_valid(&cosmetic_type, &slots) {
		return Err(UploadError::InvalidNameplateSlots);
	}
	check_slots(&cosmetic_type, &slots)?;
	if let Some(cover) = cover_data.as_deref() {
		check_cover(cover)?;
	}
	let resolved_name = resolve_name(name, &cosmetic_type, group_name.as_deref())?;

	use entities::{
		cosmetic, cosmetic_allowed_slot, cosmetic_group, cosmetic_group_allowed_slot,
		prelude::*,
	};

	let existing_group = match &group_name {
		Some(group_name) => {
			CosmeticGroup::find()
				.filter(cosmetic_group::Column::Name.eq(group_name.clone()))
				.filter(cosmetic_group::Column::Type.eq(cosmetic_type.clone()))
				.one(&state.database)
				.await?
		}
		None => None,
	};

	if let (Some(group), Some(key)) = (&existing_group, variant_key.as_deref())
		&& variant_key_taken(&state.database, group.id, key, None).await?
	{
		return Err(UploadError::DuplicateVariantKey(key.to_string()));
	}

	let (asset, renders, automatic_cover, warnings, analysis) = match (file_data, asset_from) {
		(Some(_), Some(_)) => return Err(UploadError::FileAndAssetFrom),
		(None, None) => return Err(UploadError::MissingFile),
		(Some(data), None) => {
			let stored = store_cosmetic_file(
				&state,
				&cosmetic_type,
				&slots,
				model_variant.as_deref(),
				cover_seconds,
				data,
				extension,
				content_type,
				&[variant_key.as_deref()],
			)
			.await?;
			let automatic_cover = match (stored.nameplate, &cover_data) {
				(Some(data), None) => {
					automatic_nameplate_cover(&state, data, variant_key.clone()).await
				}
				_ => None,
			};
			(
				stored.asset,
				stored.renders,
				automatic_cover,
				stored.warnings,
				stored.analysis,
			)
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
			if source.group_id.is_none()
				|| source.group_id != existing_group.as_ref().map(|group| group.id)
			{
				return Err(UploadError::AssetFromOtherGroup);
			}
			let asset = match source.asset_id {
				Some(asset_id) => {
					Asset::find_by_id(asset_id).one(&state.database).await?
				}
				None => None,
			}
			.ok_or(UploadError::InvalidAssetFrom)?;
			let automatic_cover = match stored_bundle(&state, &asset).await? {
				Some((bundle, data)) => {
					bundle.check_variant_key(variant_key.as_deref())?;
					match cover_data {
						Some(_) => None,
						None => {
							automatic_nameplate_cover(&state, data, variant_key.clone())
								.await
						}
					}
				}
				None => None,
			};
			(asset, None, automatic_cover, Vec::new(), None)
		}
	};

	let cover_asset_id = match cover_data {
		Some(cover) => Some(store_cover(&state, &cover).await?),
		None => automatic_cover.or(renders.map(|(cover, _)| cover)),
	};

	let group = match group_name {
		Some(group_name) => {
			let group = match existing_group {
				Some(group) => group,
				None => {
					cosmetic_group::ActiveModel {
						name: Set(group_name),
						r#type: Set(cosmetic_type.clone()),
						enabled: Set(listed),
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
		preview_asset_id: Set(renders.and_then(|(_, preview)| preview)),
		name: Set(Some(resolved_name)),
		r#type: Set(cosmetic_type),
		enabled: Set(row_enabled(group.is_some(), listed)),
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

	crate::api::v0::cosmetics::notify_catalog_updated(&state);
	if model.group_id.is_some() {
		sets_changed(&state).await;
	}

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
					!group.as_ref().map_or(model.enabled, |group| group.enabled),
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

	let cover_asset = match model.cover_asset_id {
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

	crate::api::v0::cosmetics::notify_catalog_updated(&state);

	Ok(Json(CreateResponse {
		cosmetic: infos.remove(0),
		warnings,
		analysis,
	}))
}

/// Uploads a cosmetic's source file, renders its cover and preview, and
/// returns the new asset with their ids, or none when rendering failed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn store_cosmetic_file(
	state: &ApiState,
	cosmetic_type: &CosmeticType,
	slots: &[BodySlot],
	model_variant: Option<&str>,
	cover_seconds: Option<f32>,
	data: Bytes,
	mut extension: String,
	mut content_type: Option<String>,
	variant_keys: &[Option<&str>],
) -> Result<StoredCosmeticFile, UploadError> {
	use entities::asset;

	let is_emote = matches!(cosmetic_type, CosmeticType::Emote);
	let is_nameplate = matches!(cosmetic_type, CosmeticType::Nameplate);
	let is_bundle = is_zip(&data);
	if matches!(cosmetic_type, CosmeticType::Effect) && !is_bundle {
		return Err(UploadError::EffectNotZip);
	}
	let limits = BundleLimits::of(cosmetic_type);
	let data: Vec<u8> = if is_bundle {
		prepare_bundle_off_thread(data, limits).await?
	} else {
		check_download(data.len(), limits)?;
		data.to_vec()
	};
	let mut warnings = Vec::new();
	let mut analysis = None;
	if is_nameplate {
		let bundle = NameplateBundle::read(&data)?;
		for key in variant_keys {
			bundle.check_variant_key(*key)?;
		}
		warnings = validate_upload(data.clone()).await?;
	} else if is_bundle && let Some(kind) = BundleKind::of(cosmetic_type) {
		let checked = check_upload(data.clone(), kind).await?;
		warnings = checked.warnings;
		analysis = checked.analysis;
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

	let color_slots = customizable_slots(&data);
	store_slots(&state.database, asset.id, color_slots.clone()).await?;
	state.color_slot_scans.insert(asset.id, color_slots).await;

	let mut nameplate = None;
	let mut render_skipped = false;
	let renders = if is_nameplate {
		nameplate = Some(data);
		None
	} else if !renders_cover(cosmetic_type) {
		None
	} else {
		match crate::api::v0::cosmetics::cover::render_cosmetic(
			&state.client,
			cosmetic_type.clone(),
			slots,
			model_variant,
			cover_seconds,
			data,
		)
		.await
		{
			Ok(renders) => match store_renders(state, &renders).await {
				Ok(ids) => Some(ids),
				Err(error) => {
					tracing::warn!("Unable to store cosmetic cover asset: {error}");
					None
				}
			},
			Err(RenderError::ManifestBundle) => {
				tracing::info!(
					"Skipping the automatic cover: bundles with a manifest.json need an uploaded cover"
				);
				render_skipped = true;
				None
			}
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

	Ok(StoredCosmeticFile {
		asset,
		renders,
		render_skipped,
		nameplate,
		warnings,
		analysis,
	})
}

pub(super) struct StoredCosmeticFile {
	pub(super) asset: entities::asset::Model,
	pub(super) renders: Option<(i32, Option<i32>)>,
	pub(super) render_skipped: bool,
	pub(super) nameplate: Option<Vec<u8>>,
	pub(super) warnings: Vec<EngineIssue>,
	pub(super) analysis: Option<BundleAnalysis>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CreateResponse {
	#[serde(flatten)]
	cosmetic: CosmeticInfo,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	warnings: Vec<EngineIssue>,
	#[serde(skip_serializing_if = "Option::is_none")]
	analysis: Option<BundleAnalysis>,
}

pub(super) async fn automatic_nameplate_cover(
	state: &ApiState,
	data: Vec<u8>,
	variant_key: Option<String>,
) -> Option<i32> {
	let cover = match render_nameplate(data, variant_key).await {
		Ok(cover) => cover?,
		Err(error) => {
			tracing::warn!("Unable to render nameplate cover: {error}");
			return None;
		}
	};
	match store_asset(
		state,
		&cover,
		Some("image/png".to_string()),
		"png",
		"covers",
	)
	.await
	{
		Ok(id) => Some(id),
		Err(error) => {
			tracing::warn!("Unable to store nameplate cover: {error}");
			None
		}
	}
}

fn listed_on_upload(cosmetic_type: &CosmeticType, requested: Option<bool>) -> bool {
	requested.unwrap_or(*cosmetic_type != CosmeticType::Nameplate)
}

fn row_enabled(grouped: bool, listed: bool) -> bool {
	grouped || listed
}

/// Stores a render's cover and, when it has one, its preview.
pub(super) async fn store_renders(
	state: &ApiState,
	renders: &Renders,
) -> Result<(i32, Option<i32>), StoreAssetError> {
	let cover =
		store_asset(state, &renders.cover, Some("image/png".to_string()), "png", "covers")
			.await?;
	let preview = match &renders.preview {
		Some(preview) => Some(
			store_asset(
				state,
				preview,
				Some(PREVIEW_CONTENT_TYPE.to_string()),
				PREVIEW_EXTENSION,
				"previews",
			)
			.await?,
		),
		None => None,
	};
	Ok((cover, preview))
}

pub(super) fn nameplate_slots_valid(kind: &CosmeticType, slots: &[BodySlot]) -> bool {
	if *kind == CosmeticType::Nameplate {
		slots == [BodySlot::Nameplate]
	} else {
		!slots.contains(&BodySlot::Nameplate)
	}
}

pub(super) async fn variant_key_taken<C: ConnectionTrait>(
	db: &C,
	group_id: i32,
	key: &str,
	except: Option<i32>,
) -> Result<bool, sea_orm::DbErr> {
	use entities::{cosmetic, prelude::*};

	let mut query = Cosmetic::find()
		.filter(cosmetic::Column::GroupId.eq(group_id))
		.filter(cosmetic::Column::VariantKey.eq(key));
	if let Some(id) = except {
		query = query.filter(cosmetic::Column::Id.ne(id));
	}
	Ok(query.one(db).await?.is_some())
}

const MAX_COVER_SIDE: u32 = 2048;

fn is_png(data: &[u8]) -> bool {
	data.starts_with(b"\x89PNG\r\n\x1a\n")
}

fn check_cover(data: &[u8]) -> Result<(), UploadError> {
	if data.len() > MAX_COVER_BYTES {
		return Err(UploadError::CoverTooLarge(data.len()));
	}
	if !is_png(data) {
		return Err(UploadError::InvalidCover);
	}
	let reader = || ImageReader::with_format(Cursor::new(data), ImageFormat::Png);
	let (width, height) = reader()
		.into_dimensions()
		.map_err(|_| UploadError::InvalidCover)?;
	let fits = |side: u32| (1..=MAX_COVER_SIDE).contains(&side);
	if !fits(width) || !fits(height) {
		return Err(UploadError::CoverDimensions(width, height));
	}
	reader().decode().map_err(|_| UploadError::InvalidCover)?;
	Ok(())
}

pub(super) async fn store_cover(
	state: &ApiState,
	data: &[u8],
) -> Result<i32, UploadError> {
	check_cover(data)?;
	Ok(crate::api::v0::collections::store_asset(
		state,
		data,
		Some("image/png".to_string()),
		"png",
		"covers",
	)
	.await?)
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use axum::{
		body::to_bytes,
		http::{StatusCode, header},
		response::IntoResponse,
	};
	use entities::sea_orm_active_enums::{BodySlot, CosmeticType};
	use image::{ImageFormat, RgbaImage};
	use serde_json::{Value, json};

	use super::{
		CreateResponse, EffectSlotError, MAX_COVER_SIDE, UploadError, check_cover, check_slots,
		listed_on_upload, nameplate_slots_valid, parse_body_slot, parse_cosmetic_type,
		resolve_name, row_enabled, storage_prefix,
	};
	use crate::api::v0::cosmetics::{
		CosmeticInfo,
		engine::{BundleAnalysis, BundleKind, CosmeticBundleError},
		nameplate::{EngineIssue, IssueLevel, NameplateBundleError},
	};

	fn png(width: u32, height: u32) -> Vec<u8> {
		let mut out = Cursor::new(Vec::new());
		RgbaImage::new(width, height).write_to(&mut out, ImageFormat::Png).unwrap();
		out.into_inner()
	}

	fn issue(code: &str, level: IssueLevel) -> EngineIssue {
		EngineIssue {
			code: code.to_string(),
			level,
			path: format!("manifest.json#layers.{code}"),
			values: [("limit".to_string(), 4.0)].into(),
			names: [("layer".to_string(), "glow".to_string())].into(),
		}
	}

	fn issue_json(code: &str, level: &str) -> Value {
		json!({
			"code": code,
			"level": level,
			"path": format!("manifest.json#layers.{code}"),
			"values": { "limit": 4.0 },
			"names": { "layer": "glow" }
		})
	}

	#[test]
	fn covers_must_decode_whole_within_the_size_limit() {
		let cover = png(40, 60);
		assert!(check_cover(&cover).is_ok());
		for broken in [&cover[..cover.len() - 20], &cover[..8], b"GIF89a"] {
			assert!(matches!(check_cover(broken), Err(UploadError::InvalidCover)));
		}
		assert!(check_cover(&png(MAX_COVER_SIDE, 1)).is_ok());
		assert!(matches!(
			check_cover(&png(MAX_COVER_SIDE + 1, 1)),
			Err(UploadError::CoverDimensions(width, 1)) if width == MAX_COVER_SIDE + 1
		));
	}

	#[test]
	fn types_take_only_the_slots_they_fit() {
		use BodySlot::{Backpack, Hat, Nameplate, Tail};
		use CosmeticType as T;
		assert!(nameplate_slots_valid(&T::Nameplate, &[Nameplate]));
		assert!(!nameplate_slots_valid(&T::Nameplate, &[Nameplate, Hat]));
		assert!(!nameplate_slots_valid(&T::Nameplate, &[]));
		assert!(nameplate_slots_valid(&T::Hat, &[Hat]));
		assert!(!nameplate_slots_valid(&T::Hat, &[Hat, Nameplate]));
		assert!(!nameplate_slots_valid(&T::Cape, &[Nameplate]));
		assert!(nameplate_slots_valid(&T::Emote, &[]));

		for worn in [T::Backpack, T::Wings, T::Aura] {
			assert_eq!(check_slots(&worn, &[Tail]), Ok(()));
			assert_eq!(check_slots(&worn, &[Backpack, Tail]), Ok(()));
		}
		assert_eq!(check_slots(&T::Effect, &[Tail]), Err(EffectSlotError::EffectOutsideEffectSlots));

		assert_eq!(parse_cosmetic_type("effect"), Some(T::Effect));
		for (name, slot) in [("effect_a", Some(BodySlot::EffectA)), ("effect_b", Some(BodySlot::EffectB)), ("effect", None), ("tail", Some(Tail))] {
			assert_eq!(parse_body_slot(name), slot, "{name}");
		}
		assert_eq!(storage_prefix(&T::Effect), "effects");
		assert_eq!(serde_json::to_value(Tail).unwrap(), "tail");
		assert_eq!(serde_json::from_value::<BodySlot>(json!("tail")).unwrap(), Tail);
		assert_eq!(sea_orm::ActiveEnum::to_value(&Tail), "tail");
	}

	#[test]
	fn listing_names_and_visibility_follow_the_upload() {
		use CosmeticType as T;
		for (kind, listed, expected) in [
			(T::Nameplate, None, false),
			(T::Nameplate, Some(true), true),
			(T::Nameplate, Some(false), false),
			(T::Cape, None, true),
			(T::Emote, None, true),
			(T::Cape, Some(false), false),
		] {
			assert_eq!(listed_on_upload(&kind, listed), expected, "{kind:?} {listed:?}");
		}
		assert!(row_enabled(true, false) && row_enabled(true, true) && row_enabled(false, true));
		assert!(!row_enabled(false, false));

		assert_eq!(resolve_name(Some("Sparks".into()), &T::Effect, None).unwrap(), "Sparks");
		assert_eq!(resolve_name(None, &T::Effect, Some("Embers")).unwrap(), "Embers");
		assert!(matches!(resolve_name(None, &T::Effect, None), Err(UploadError::MissingName)));
		assert_eq!(resolve_name(None, &T::Pet, Some("Cats")).unwrap(), "Pet");
	}

	#[test]
	fn the_create_response_adds_warnings_and_analysis_only_when_present() {
		let created = |warnings, analysis| {
			let cosmetic = CosmeticInfo {
				id: 77,
				grouped: true,
				r#type: CosmeticType::Nameplate,
				name: "Pride".to_string(),
				allowed_slots: vec![BodySlot::Nameplate],
				variants: Vec::new(),
			};
			serde_json::to_value(CreateResponse { cosmetic, warnings, analysis }).unwrap()
		};
		assert_eq!(
			created(Vec::new(), None),
			json!({
				"id": 77,
				"grouped": true,
				"type": "nameplate",
				"name": "Pride",
				"allowed_slots": ["nameplate"],
				"variants": []
			})
		);
		let warned = created(vec![issue("readability", IssueLevel::Warning), issue("cost", IssueLevel::Info)], None);
		assert_eq!(warned["warnings"], json!([issue_json("readability", "warning"), issue_json("cost", "info")]));

		let analysis = BundleAnalysis {
			requires: vec!["pets".to_string(), "springs".to_string()],
			quads: 40,
			bones: 6,
			spring_bones: 2,
			live_particles: 0.0,
			trail_points: 0,
			record_bytes: 96,
			texture_bytes: 4096,
		};
		let analysed = created(Vec::new(), Some(analysis));
		assert_eq!(analysed.get("warnings"), None);
		assert_eq!(
			analysed["analysis"],
			json!({
				"requires": ["pets", "springs"],
				"quads": 40,
				"bones": 6,
				"spring_bones": 2,
				"live_particles": 0.0,
				"trail_points": 0,
				"record_bytes": 96,
				"texture_bytes": 4096
			})
		);
	}

	#[tokio::test]
	async fn rejected_bundles_return_their_issues_as_json_and_other_errors_stay_text() {
		for error in [
			UploadError::Nameplate(NameplateBundleError::Invalid(vec![issue("unknown_key", IssueLevel::Error)])),
			UploadError::Bundle(CosmeticBundleError::Invalid(vec![issue("unknown_key", IssueLevel::Error)])),
		] {
			let response = error.into_response();
			assert_eq!(response.status(), StatusCode::BAD_REQUEST);
			assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
			let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
			assert!(body["message"].is_string());
			assert_eq!(body["issues"], json!([issue_json("unknown_key", "error")]));
		}
		for error in [
			UploadError::Bundle(CosmeticBundleError::WrongKind { expected: BundleKind::Item, found: BundleKind::Pet }),
			UploadError::Bundle(CosmeticBundleError::NestedManifest("wings/manifest.json".to_string())),
			UploadError::InvalidNameplateSlots,
		] {
			let response = error.into_response();
			assert_eq!(response.status(), StatusCode::BAD_REQUEST);
			let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
			assert!(serde_json::from_slice::<Value>(&body).is_err());
		}
	}
}
