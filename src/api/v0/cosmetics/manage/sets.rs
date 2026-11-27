use std::collections::{BTreeSet, HashMap};

use aide::{
	OperationInput, OperationIo,
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{
		DefaultBodyLimit, FromRequest, Multipart, Request, State,
		multipart::MultipartRejection,
	},
	http::StatusCode,
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset, Utc};
use entities::{
	asset, cosmetic, cosmetic_group, cosmetic_set, cosmetic_set_requirement, prelude::*,
	sea_orm_active_enums::AssetKind,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue::NotSet, ColumnTrait, ConnectionTrait, EntityTrait,
	ModelTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::create::{InvalidBundleBody, variant_key_taken};
use crate::{
	api::{
		ApiState,
		permissions::{Require, perm},
		v0::{
			bonuses::{admin_preview, admin_preview_doc, sets_changed},
			cosmetics::{
				CachedAssetInfo,
				engine::{BundleAnalysis, BundleKind, CosmeticBundleError, check_upload},
				limits::{
					BundleLimitError, BundleLimits, SET_EFFECT_BODY_LIMIT,
					prepare_bundle_off_thread,
				},
				nameplate::EngineIssue,
			},
		},
	},
	utils::{
		hash::sha256_hex,
		zip::is_zip,
	},
};

const MAX_NAME_LENGTH: usize = 64;
const MAX_REQUIREMENTS: usize = 8;
const MAX_VARIANT_KEY_LENGTH: usize = 64;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SetError {
	#[error("The set needs a name of 1 to {MAX_NAME_LENGTH} characters")]
	InvalidName,
	#[error("A set needs 1 to {MAX_REQUIREMENTS} requirements")]
	RequirementCount,
	#[error("Each requirement needs exactly one of cosmetic_id or group_id")]
	AmbiguousRequirement,
	#[error(
		"A variant_key needs a group_id and must be 1 to {MAX_VARIANT_KEY_LENGTH} \
		 characters"
	)]
	InvalidVariantKey,
	#[error("active_from must be before active_until")]
	InvalidWindow,
	#[error("Cosmetic {0} does not exist")]
	MissingCosmetic(i32),
	#[error("Cosmetic group {0} does not exist")]
	MissingGroup(i32),
	#[error("Cosmetic group {0} has no variant with the key {1:?}")]
	MissingVariantKey(i32, String),
	#[error("Asset {0} does not exist")]
	MissingAsset(i32),
	#[error("The requested set does not exist")]
	MissingSet,
	#[error("Missing set_id in multipart data")]
	MissingSetId,
	#[error("Missing file in multipart data")]
	MissingFile,
	#[error("The effect must be a zip bundle")]
	NotABundle,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("S3 error: {0}")]
	S3(#[from] s3::error::S3Error),
	#[error("Multipart error: {0}")]
	Multipart(#[from] axum::extract::multipart::MultipartError),
	#[error("Multipart rejection: {0}")]
	Rejection(#[from] MultipartRejection),
	#[error(transparent)]
	EffectBundle(#[from] BundleLimitError),
	#[error(transparent)]
	Bundle(#[from] CosmeticBundleError),
}

impl IntoResponse for SetError {
	fn into_response(self) -> axum::response::Response {
		if let Self::Bundle(CosmeticBundleError::Invalid(issues)) = &self {
			let body = InvalidBundleBody {
				message: self.to_string(),
				issues: issues.clone(),
			};
			return (StatusCode::BAD_REQUEST, Json(body)).into_response();
		}
		crate::api::admin_error_response(
			match self {
				Self::Bundle(CosmeticBundleError::ValidatorFailed(_))
				| Self::EffectBundle(BundleLimitError::Stopped(_)) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
				Self::InvalidName
				| Self::RequirementCount
				| Self::AmbiguousRequirement
				| Self::InvalidVariantKey
				| Self::InvalidWindow
				| Self::MissingCosmetic(_)
				| Self::MissingGroup(_)
				| Self::MissingVariantKey(..)
				| Self::MissingAsset(_)
				| Self::MissingSetId
				| Self::MissingFile
				| Self::NotABundle
				| Self::EffectBundle(_)
				| Self::Bundle(_)
				| Self::Rejection(_) => StatusCode::BAD_REQUEST,
				Self::MissingSet => StatusCode::NOT_FOUND,
				Self::Multipart(ref e) => e.status(),
				Self::Database(_) | Self::S3(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
			},
			self,
		)
	}
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
struct RequirementBody {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	cosmetic_id: Option<i32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	group_id: Option<i32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	variant_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetBody {
	name: String,
	#[serde(default)]
	priority: i32,
	#[serde(default)]
	effect_asset_id: Option<i32>,
	#[serde(default)]
	active_from: Option<DateTime<Utc>>,
	#[serde(default)]
	active_until: Option<DateTime<Utc>>,
	requirements: Vec<RequirementBody>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateSetRequest {
	id: i32,
	#[serde(flatten)]
	set: SetBody,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteSetRequest {
	id: i32,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SetInfo {
	id: i32,
	name: String,
	priority: i32,
	effect_asset_id: Option<i32>,
	effect_url: Option<String>,
	active_from: Option<DateTime<FixedOffset>>,
	active_until: Option<DateTime<FixedOffset>>,
	created_at: DateTime<FixedOffset>,
	requirements: Vec<RequirementBody>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SetEffectResponse {
	#[serde(flatten)]
	set: SetInfo,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	warnings: Vec<EngineIssue>,
	#[serde(skip_serializing_if = "Option::is_none")]
	analysis: Option<BundleAnalysis>,
}

#[derive(Debug, PartialEq, Eq)]
struct ValidSet {
	name: String,
	priority: i32,
	effect_asset_id: Option<i32>,
	active_from: Option<DateTime<Utc>>,
	active_until: Option<DateTime<Utc>>,
	requirements: Vec<RequirementBody>,
}

fn validate(body: SetBody) -> Result<ValidSet, SetError> {
	let name = body.name.trim();
	if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
		return Err(SetError::InvalidName);
	}
	if body.requirements.is_empty() || body.requirements.len() > MAX_REQUIREMENTS {
		return Err(SetError::RequirementCount);
	}
	if let (Some(from), Some(until)) = (body.active_from, body.active_until)
		&& from >= until
	{
		return Err(SetError::InvalidWindow);
	}

	let requirements = body
		.requirements
		.into_iter()
		.map(|requirement| {
			if requirement.cosmetic_id.is_some() == requirement.group_id.is_some() {
				return Err(SetError::AmbiguousRequirement);
			}
			let variant_key = match requirement.variant_key {
				None => None,
				Some(key) => {
					let key = key.trim();
					if requirement.group_id.is_none()
						|| key.is_empty() || key.chars().count() > MAX_VARIANT_KEY_LENGTH
					{
						return Err(SetError::InvalidVariantKey);
					}
					Some(key.to_owned())
				}
			};
			Ok(RequirementBody {
				variant_key,
				..requirement
			})
		})
		.collect::<Result<Vec<_>, _>>()?;

	Ok(ValidSet {
		name: name.to_owned(),
		priority: body.priority,
		effect_asset_id: body.effect_asset_id,
		active_from: body.active_from,
		active_until: body.active_until,
		requirements,
	})
}

async fn check_references<C: ConnectionTrait>(
	db: &C,
	set: &ValidSet,
) -> Result<(), SetError> {
	let cosmetic_ids: BTreeSet<i32> = set
		.requirements
		.iter()
		.filter_map(|requirement| requirement.cosmetic_id)
		.collect();
	if !cosmetic_ids.is_empty() {
		let found: BTreeSet<i32> = Cosmetic::find()
			.filter(cosmetic::Column::Id.is_in(cosmetic_ids.iter().copied()))
			.all(db)
			.await?
			.into_iter()
			.map(|cosmetic| cosmetic.id)
			.collect();
		if let Some(missing) = cosmetic_ids.difference(&found).next() {
			return Err(SetError::MissingCosmetic(*missing));
		}
	}

	let group_ids: BTreeSet<i32> = set
		.requirements
		.iter()
		.filter_map(|requirement| requirement.group_id)
		.collect();
	if !group_ids.is_empty() {
		let found: BTreeSet<i32> = CosmeticGroup::find()
			.filter(cosmetic_group::Column::Id.is_in(group_ids.iter().copied()))
			.all(db)
			.await?
			.into_iter()
			.map(|group| group.id)
			.collect();
		if let Some(missing) = group_ids.difference(&found).next() {
			return Err(SetError::MissingGroup(*missing));
		}
	}

	for requirement in &set.requirements {
		if let (Some(group_id), Some(key)) =
			(requirement.group_id, requirement.variant_key.as_deref())
			&& !variant_key_taken(db, group_id, key, None).await?
		{
			return Err(SetError::MissingVariantKey(group_id, key.to_owned()));
		}
	}

	if let Some(asset_id) = set.effect_asset_id
		&& Asset::find_by_id(asset_id).one(db).await?.is_none()
	{
		return Err(SetError::MissingAsset(asset_id));
	}

	Ok(())
}

fn requirement_body(row: &cosmetic_set_requirement::Model) -> RequirementBody {
	RequirementBody {
		cosmetic_id: row.cosmetic_id,
		group_id: row.group_id,
		variant_key: row.variant_key.clone(),
	}
}

fn to_info(
	set: cosmetic_set::Model,
	mut requirements: Vec<cosmetic_set_requirement::Model>,
	assets: &HashMap<i32, asset::Model>,
	public_url: &str,
) -> SetInfo {
	requirements.sort_by_key(|requirement| requirement.id);
	SetInfo {
		effect_url: CachedAssetInfo::asset_url(
			set.effect_asset_id.and_then(|id| assets.get(&id)),
			public_url,
		),
		id: set.id,
		name: set.name,
		priority: set.priority,
		effect_asset_id: set.effect_asset_id,
		active_from: set.active_from,
		active_until: set.active_until,
		created_at: set.created_at,
		requirements: requirements.iter().map(requirement_body).collect(),
	}
}

async fn load_infos(state: &ApiState, id: Option<i32>) -> Result<Vec<SetInfo>, SetError> {
	let mut query = CosmeticSet::find();
	if let Some(id) = id {
		query = query.filter(cosmetic_set::Column::Id.eq(id));
	}
	let rows = query
		.order_by_desc(cosmetic_set::Column::Priority)
		.order_by_asc(cosmetic_set::Column::Id)
		.find_with_related(CosmeticSetRequirement)
		.all(&state.database)
		.await?;

	let asset_ids: Vec<i32> = rows
		.iter()
		.filter_map(|(set, _)| set.effect_asset_id)
		.collect();
	let assets: HashMap<i32, asset::Model> = if asset_ids.is_empty() {
		HashMap::new()
	} else {
		Asset::find()
			.filter(asset::Column::Id.is_in(asset_ids))
			.all(&state.database)
			.await?
			.into_iter()
			.map(|asset| (asset.id, asset))
			.collect()
	};

	Ok(rows
		.into_iter()
		.map(|(set, requirements)| {
			to_info(set, requirements, &assets, &state.s3_public_url)
		})
		.collect())
}

async fn load_info(state: &ApiState, id: i32) -> Result<SetInfo, SetError> {
	load_infos(state, Some(id))
		.await?
		.pop()
		.ok_or(SetError::MissingSet)
}

async fn write_requirements<C: ConnectionTrait>(
	db: &C,
	set_id: i32,
	requirements: &[RequirementBody],
) -> Result<(), SetError> {
	CosmeticSetRequirement::delete_many()
		.filter(cosmetic_set_requirement::Column::SetId.eq(set_id))
		.exec(db)
		.await?;
	CosmeticSetRequirement::insert_many(requirements.iter().map(|requirement| {
		cosmetic_set_requirement::ActiveModel {
			id: NotSet,
			set_id: Set(set_id),
			cosmetic_id: Set(requirement.cosmetic_id),
			group_id: Set(requirement.group_id),
			variant_key: Set(requirement.variant_key.clone()),
		}
	}))
	.exec(db)
	.await?;
	Ok(())
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listCosmeticSets")
		.summary("List cosmetic sets")
		.description(
			"Lists every cosmetic set with its requirements, highest priority \
			 first. Requires `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createCosmeticSet")
		.summary("Create a cosmetic set")
		.description(
			"Creates a set: a list of requirements that, all worn at once, give \
			 the wearer the set's effect. A requirement names a cosmetic, or a \
			 group with an optional `variant_key` to pin one colourway. Up to two \
			 sets apply at a time, highest `priority` first, then lowest id. \
			 `active_from` and `active_until` limit a set to a time window. \
			 Requires `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description("The set is invalid or names something that does not exist")
		})
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateCosmeticSet")
		.summary("Replace a cosmetic set")
		.description(
			"Replaces a set's fields and requirements. Players who already found \
			 it keep their discovery. Requires `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No set exists with the given id")
		})
}

fn delete_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteCosmeticSet")
		.summary("Delete a cosmetic set")
		.description(
			"Deletes a set, its requirements and every player's discovery of it. \
			 Requires `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The set was deleted")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No set exists with the given id")
		})
}

fn effect_doc(op: TransformOperation) -> TransformOperation {
	op.id("uploadCosmeticSetEffect")
		.summary("Upload a cosmetic set's effect")
		.description(
			"Uploads the effect bundle (a zip) worn by players who complete the \
			 set, and points the set at it. Returns the set, plus any `warnings` \
			 and the `analysis` the bundle check raised. Requires \
			 `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No set exists with the given id")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/sets", get_with(self::list, self::list_doc))
		.api_route("/sets/create", post_with(self::create, self::create_doc))
		.api_route("/sets/update", post_with(self::update, self::update_doc))
		.api_route("/sets/delete", post_with(self::delete, self::delete_doc))
		.api_route("/sets/preview", post_with(admin_preview, admin_preview_doc))
		.merge(
			ApiRouter::new()
				.api_route("/sets/effect", post_with(self::effect, self::effect_doc))
				.layer(DefaultBodyLimit::max(SET_EFFECT_BODY_LIMIT)),
		)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn list(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
) -> Result<Json<Vec<SetInfo>>, SetError> {
	Ok(Json(load_infos(&state, None).await?))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Json(body): Json<SetBody>,
) -> Result<Json<SetInfo>, SetError> {
	let set = validate(body)?;
	check_references(&state.database, &set).await?;

	let txn = state.database.begin().await?;
	let created = cosmetic_set::ActiveModel {
		id: NotSet,
		name: Set(set.name),
		priority: Set(set.priority),
		effect_asset_id: Set(set.effect_asset_id),
		active_from: Set(set.active_from.map(Into::into)),
		active_until: Set(set.active_until.map(Into::into)),
		created_at: NotSet,
	}
	.insert(&txn)
	.await?;
	write_requirements(&txn, created.id, &set.requirements).await?;
	txn.commit().await?;

	sets_changed(&state).await;
	Ok(Json(load_info(&state, created.id).await?))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn update(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Json(body): Json<UpdateSetRequest>,
) -> Result<Json<SetInfo>, SetError> {
	let set = validate(body.set)?;
	check_references(&state.database, &set).await?;

	let txn = state.database.begin().await?;
	let existing = CosmeticSet::find_by_id(body.id)
		.one(&txn)
		.await?
		.ok_or(SetError::MissingSet)?;
	let mut active: cosmetic_set::ActiveModel = existing.into();
	active.name = Set(set.name);
	active.priority = Set(set.priority);
	active.effect_asset_id = Set(set.effect_asset_id);
	active.active_from = Set(set.active_from.map(Into::into));
	active.active_until = Set(set.active_until.map(Into::into));
	active.update(&txn).await?;
	write_requirements(&txn, body.id, &set.requirements).await?;
	txn.commit().await?;

	sets_changed(&state).await;
	Ok(Json(load_info(&state, body.id).await?))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn delete(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Json(body): Json<DeleteSetRequest>,
) -> Result<StatusCode, SetError> {
	let set = CosmeticSet::find_by_id(body.id)
		.one(&state.database)
		.await?
		.ok_or(SetError::MissingSet)?;
	set.delete(&state.database).await?;

	sets_changed(&state).await;
	Ok(StatusCode::NO_CONTENT)
}

struct EffectUpload(Multipart);

impl<S> FromRequest<S> for EffectUpload
where
	S: Send + Sync,
{
	type Rejection = SetError;

	async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
		Ok(Self(
			Multipart::from_request(req, state)
				.await
				.map_err(SetError::Rejection)?,
		))
	}
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct EffectUploadRequest {
	set_id: i32,
	#[schemars(with = "String")]
	file: String,
}

impl OperationInput for EffectUpload {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.request_body = Some(aide::openapi::ReferenceOr::Item(
			aide::openapi::RequestBody {
				description: Some("Multipart effect upload".into()),
				content: [(
					"multipart/form-data".into(),
					aide::openapi::MediaType {
						schema: Some(aide::openapi::SchemaObject {
							json_schema: ctx
								.schema
								.subschema_for::<EffectUploadRequest>(),
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

#[tracing::instrument(level = "debug", skip(state, _auth, multipart))]
async fn effect(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	EffectUpload(mut multipart): EffectUpload,
) -> Result<Json<SetEffectResponse>, SetError> {
	let mut set_id = None;
	let mut file = None;
	while let Some(field) = multipart.next_field().await? {
		match field.name() {
			Some("set_id") => {
				set_id = field.text().await?.trim().parse::<i32>().ok();
			}
			Some("file") => file = Some(field.bytes().await?),
			_ => {}
		}
	}
	let set_id = set_id.ok_or(SetError::MissingSetId)?;
	let file = file.ok_or(SetError::MissingFile)?;
	if !is_zip(&file) {
		return Err(SetError::NotABundle);
	}
	let data = prepare_bundle_off_thread(file, BundleLimits::EFFECT).await?;
	let checked = check_upload(data.clone(), BundleKind::Effect).await?;

	let set = CosmeticSet::find_by_id(set_id)
		.one(&state.database)
		.await?
		.ok_or(SetError::MissingSet)?;

	let path = format!("effects/{}.zip", Uuid::now_v7());
	state
		.s3_bucket
		.put_object_with_content_type(&path, &data, "application/zip")
		.await?;
	let asset = asset::ActiveModel {
		storage_path: Set(Some(path)),
		url: Set(None),
		asset_kind: Set(AssetKind::Bundle),
		content_type: Set(Some("application/zip".to_owned())),
		hash: Set(Some(sha256_hex(&data))),
		..Default::default()
	}
	.insert(&state.database)
	.await?;
	let info = CachedAssetInfo::from_db_model(&asset, state.s3_bucket.clone()).await?;
	state.asset_cache.insert(asset.id, info).await;

	let mut active: cosmetic_set::ActiveModel = set.into();
	active.effect_asset_id = Set(Some(asset.id));
	active.update(&state.database).await?;

	sets_changed(&state).await;
	Ok(Json(SetEffectResponse {
		set: load_info(&state, set_id).await?,
		warnings: checked.warnings,
		analysis: checked.analysis,
	}))
}

#[cfg(test)]
mod tests {
	use axum::{
		body::to_bytes,
		http::{StatusCode, header},
		response::IntoResponse,
	};
	use chrono::{Duration, TimeZone, Utc};
	use serde_json::{Value, json};

	use super::{RequirementBody, SetBody, SetEffectResponse, SetError, SetInfo, validate};
	use crate::api::v0::cosmetics::{
		engine::{BundleAnalysis, BundleKind, CosmeticBundleError},
		limits::{BundleLimitError, BundleLimits},
		nameplate::{EngineIssue, IssueLevel},
	};

	fn cosmetic(id: i32) -> RequirementBody {
		RequirementBody { cosmetic_id: Some(id), ..Default::default() }
	}

	fn group(id: i32, variant_key: Option<&str>) -> RequirementBody {
		RequirementBody { group_id: Some(id), variant_key: variant_key.map(str::to_owned), ..Default::default() }
	}

	fn body(requirements: Vec<RequirementBody>) -> SetBody {
		SetBody {
			name: "  Night Watch ".to_owned(),
			priority: 3,
			effect_asset_id: Some(9),
			active_from: None,
			active_until: None,
			requirements,
		}
	}

	fn issue(code: &str, level: IssueLevel) -> EngineIssue {
		let (code, path) = (code.to_string(), "manifest.json#emitters".to_string());
		EngineIssue { code, level, path, values: Default::default(), names: Default::default() }
	}

	#[test]
	fn sets_are_validated_and_normalized() {
		let set = validate(body(vec![cosmetic(1), group(2, Some(" red "))])).expect("the set is valid");
		assert_eq!((set.name.as_str(), set.priority), ("Night Watch", 3));
		assert_eq!(set.requirements, vec![cosmetic(1), group(2, Some("red"))]);
		assert!(validate(body((0..8).map(cosmetic).collect())).is_ok());
		assert!(validate(body(vec![group(2, None), group(2, None)])).is_ok());

		let start = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
		let windowed = |until| SetBody { active_from: Some(start), active_until: Some(until), ..body(vec![cosmetic(1)]) };
		assert!(validate(windowed(start + Duration::days(7))).is_ok());
		let named = |name: String| SetBody { name, ..body(vec![cosmetic(1)]) };
		let both = RequirementBody { cosmetic_id: Some(1), group_id: Some(2), variant_key: None };
		let keyed = RequirementBody { cosmetic_id: Some(1), group_id: None, variant_key: Some("red".to_owned()) };
		for (set, expected) in [
			(named("   ".to_owned()), SetError::InvalidName),
			(named("x".repeat(65)), SetError::InvalidName),
			(body(Vec::new()), SetError::RequirementCount),
			(body((0..9).map(cosmetic).collect()), SetError::RequirementCount),
			(body(vec![RequirementBody::default()]), SetError::AmbiguousRequirement),
			(body(vec![both]), SetError::AmbiguousRequirement),
			(body(vec![keyed]), SetError::InvalidVariantKey),
			(body(vec![group(2, Some("  "))]), SetError::InvalidVariantKey),
			(windowed(start), SetError::InvalidWindow),
		] {
			let error = validate(set).expect_err("the set is invalid");
			assert_eq!(std::mem::discriminant(&error), std::mem::discriminant(&expected), "{error:?}");
		}

		let parsed: SetBody = serde_json::from_str(
			r#"{"name":"Duo","requirements":[{"cosmetic_id":1},{"group_id":2,"variant_key":"red"}]}"#,
		)
		.unwrap();
		assert_eq!((parsed.priority, parsed.effect_asset_id), (0, None));
		assert_eq!(parsed.requirements, vec![cosmetic(1), group(2, Some("red"))]);
	}

	#[test]
	fn an_effect_upload_returns_the_set_with_its_check() {
		let uploaded = |warnings, analysis| {
			let created_at = Utc.with_ymd_and_hms(2026, 11, 25, 12, 0, 0).unwrap().fixed_offset();
			let set = SetInfo {
				id: 3,
				name: "Winter".to_string(),
				priority: 1,
				effect_asset_id: Some(9),
				effect_url: Some("https://cdn.example.com/effects/a.zip".to_string()),
				active_from: None,
				active_until: None,
				created_at,
				requirements: vec![cosmetic(1)],
			};
			serde_json::to_value(SetEffectResponse { set, warnings, analysis }).unwrap()
		};
		assert_eq!(
			uploaded(Vec::new(), None),
			json!({
				"id": 3,
				"name": "Winter",
				"priority": 1,
				"effect_asset_id": 9,
				"effect_url": "https://cdn.example.com/effects/a.zip",
				"active_from": null,
				"active_until": null,
				"created_at": "2026-11-25T12:00:00Z",
				"requirements": [{ "cosmetic_id": 1 }]
			})
		);
		let analysis = BundleAnalysis { live_particles: 12.0, trail_points: 64, ..Default::default() };
		let checked = uploaded(vec![issue("unreferenced_file", IssueLevel::Warning)], Some(analysis));
		assert_eq!((&checked["warnings"][0]["code"], &checked["warnings"][0]["level"]), (&json!("unreferenced_file"), &json!("warning")));
		assert_eq!((&checked["analysis"]["live_particles"], &checked["analysis"]["trail_points"]), (&json!(12.0), &json!(64)));
	}

	#[tokio::test]
	async fn bad_effect_bundles_are_bad_requests_with_issues_as_json() {
		let response = SetError::Bundle(CosmeticBundleError::Invalid(vec![issue("particle_limit", IssueLevel::Error)])).into_response();
		assert_eq!(response.status(), StatusCode::BAD_REQUEST);
		assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
		let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
		assert_eq!((&body["issues"][0]["code"], &body["issues"][0]["level"]), (&json!("particle_limit"), &json!("error")));

		let too_large = BundleLimitError::TooLargeUnpacked { size: BundleLimits::EFFECT.unpacked + 1, max: BundleLimits::EFFECT.unpacked };
		let wrong_kind = CosmeticBundleError::WrongKind { expected: BundleKind::Effect, found: BundleKind::Item };
		for error in [SetError::EffectBundle(too_large), SetError::Bundle(wrong_kind)] {
			assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
		}
	}
}
