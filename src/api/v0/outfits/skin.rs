use aide::{
	axum::{
		ApiRouter,
		routing::{get_with, patch_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use entities::{player_skin, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
	QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};

use super::{
	OutfitError, SKIN_LIMIT, SkinInfo, SkinModel, lock_player, next_numbered_name,
	normalize_texture_hash, validate_name,
};
use crate::api::{ApiState, v0::account::AuthenticatedPlayer};

const SKIN_NAME_PREFIX: &str = "Skin #";

#[derive(Debug, Serialize, JsonSchema)]
struct SkinListResponse {
	skins: Vec<SkinInfo>,
	limit: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddSkinRequest {
	texture_hash: String,
	model: Option<SkinModel>,
	name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateSkinRequest {
	name: Option<String>,
	favorite: Option<bool>,
	model: Option<SkinModel>,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listSkins")
		.summary("List the player's skins")
		.description("Lists the authenticated player's skin library, newest first.")
		.tag("outfits")
}

fn add_doc(op: TransformOperation) -> TransformOperation {
	op.id("addSkin")
		.summary("Add a skin to the library")
		.description(
			"Records a Mojang skin texture in the library by its `texture_hash` (32 to \
			 64 hexadecimal characters, the last path segment of its \
			 `textures.minecraft.net` URL), with an optional `model` (`classic` or \
			 `slim`, default `classic`) and `name`. Names default to `Skin #N`. \
			 Adding a texture already in the library returns the existing entry \
			 unchanged with `200`, even when a different `model` is given; use \
			 `PATCH /skins/{id}` to change its model.",
		)
		.tag("outfits")
		.response_with::<{ StatusCode::CREATED.as_u16() }, Json<SkinInfo>, _>(|res| {
			res.description("The skin was added to the library")
		})
		.response_with::<{ StatusCode::OK.as_u16() }, Json<SkinInfo>, _>(|res| {
			res.description("The skin was already in the library")
		})
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description("The texture hash or name is invalid")
		})
		.response_with::<{ StatusCode::CONFLICT.as_u16() }, String, _>(|res| {
			res.description("The player already has the maximum number of skins")
		})
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateSkin")
		.summary("Update a skin")
		.description("Renames a skin, toggles its favorite flag or changes its model.")
		.tag("outfits")
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("The skin does not exist or is not the player's")
		})
}

fn delete_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteSkin")
		.summary("Delete a skin")
		.description(
			"Removes a skin from the library. Outfits using it are left without a skin.",
		)
		.tag("outfits")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The skin was deleted")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("The skin does not exist or is not the player's")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/skins",
			get_with(self::list, self::list_doc).post_with(self::add, self::add_doc),
		)
		.api_route(
			"/skins/{id}",
			patch_with(self::update, self::update_doc)
				.delete_with(self::delete, self::delete_doc),
		)
}

async fn find_skin<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	id: i32,
) -> Result<player_skin::Model, OutfitError> {
	PlayerSkin::find_by_id(id)
		.filter(player_skin::Column::PlayerId.eq(player_id))
		.one(db)
		.await?
		.ok_or(OutfitError::SkinMissing)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<SkinListResponse>, OutfitError> {
	let skins = PlayerSkin::find()
		.filter(player_skin::Column::PlayerId.eq(player.id))
		.order_by_desc(player_skin::Column::CreatedAt)
		.order_by_desc(player_skin::Column::Id)
		.all(&state.database)
		.await?;

	Ok(Json(SkinListResponse {
		skins: skins.into_iter().map(SkinInfo::from_model).collect(),
		limit: SKIN_LIMIT,
	}))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn add(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<AddSkinRequest>,
) -> Result<(StatusCode, Json<SkinInfo>), OutfitError> {
	let hash = normalize_texture_hash(&body.texture_hash)?;
	let name = body
		.name
		.as_deref()
		.filter(|name| !name.trim().is_empty())
		.map(validate_name)
		.transpose()?;
	let model = body.model.unwrap_or(SkinModel::Classic);

	let txn = state.database.begin().await?;
	lock_player(&txn, &player).await?;

	let existing = PlayerSkin::find()
		.filter(player_skin::Column::PlayerId.eq(player.id))
		.all(&txn)
		.await?;
	if let Some(skin) = existing.iter().find(|skin| skin.hash == hash) {
		return Ok((StatusCode::OK, Json(SkinInfo::from_model(skin.clone()))));
	}
	if existing.len() as u64 >= SKIN_LIMIT {
		return Err(OutfitError::SkinLimit);
	}

	let skin = player_skin::ActiveModel {
		player_id: Set(player.id),
		name: Set(name.unwrap_or_else(|| {
			next_numbered_name(
				SKIN_NAME_PREFIX,
				existing.iter().map(|skin| skin.name.as_str()),
			)
		})),
		model: Set(model.as_str().to_string()),
		hash: Set(hash),
		favorite: Set(false),
		..Default::default()
	}
	.insert(&txn)
	.await?;

	txn.commit().await?;

	Ok((StatusCode::CREATED, Json(SkinInfo::from_model(skin))))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn update(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
	Json(body): Json<UpdateSkinRequest>,
) -> Result<Json<SkinInfo>, OutfitError> {
	let name = body.name.as_deref().map(validate_name).transpose()?;

	let txn = state.database.begin().await?;
	lock_player(&txn, &player).await?;
	let skin = find_skin(&txn, player.id, id).await?;

	let mut active: player_skin::ActiveModel = skin.into();
	if let Some(name) = name {
		active.name = Set(name);
	}
	if let Some(favorite) = body.favorite {
		active.favorite = Set(favorite);
	}
	if let Some(model) = body.model {
		active.model = Set(model.as_str().to_string());
	}
	active.updated_at = Set(chrono::Utc::now().fixed_offset());
	let skin = active.update(&txn).await?;
	txn.commit().await?;

	Ok(Json(SkinInfo::from_model(skin)))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn delete(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, OutfitError> {
	let txn = state.database.begin().await?;
	lock_player(&txn, &player).await?;
	let skin = find_skin(&txn, player.id, id).await?;
	PlayerSkin::delete_by_id(skin.id).exec(&txn).await?;
	txn.commit().await?;

	Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
	use super::{
		SKIN_NAME_PREFIX,
		super::{next_numbered_name, normalize_texture_hash, texture_url},
	};

	#[test]
	fn names_skins_after_the_lowest_gap() {
		assert_eq!(
			next_numbered_name(SKIN_NAME_PREFIX, ["Skin #1", "Skin #3", "Outfit #2"]),
			"Skin #2"
		);
	}

	#[test]
	fn accepts_mojang_texture_hashes() {
		let hash = "1a4af718455d4aab528e7a61f86fa25e6a369d1768dcb13f7df319a713eb810b";
		assert_eq!(normalize_texture_hash(hash).unwrap(), hash);
		assert_eq!(
			normalize_texture_hash(&hash.to_ascii_uppercase()).unwrap(),
			hash
		);
		assert!(normalize_texture_hash(&"a".repeat(32)).is_ok());
	}

	#[test]
	fn rejects_invalid_texture_hashes() {
		assert!(normalize_texture_hash("").is_err());
		assert!(normalize_texture_hash(&"a".repeat(31)).is_err());
		assert!(normalize_texture_hash(&"a".repeat(65)).is_err());
		assert!(normalize_texture_hash(&"g".repeat(32)).is_err());
		assert!(normalize_texture_hash(&format!("{}/x", "a".repeat(32))).is_err());
	}

	#[test]
	fn builds_texture_urls() {
		assert_eq!(
			texture_url("abc"),
			"https://textures.minecraft.net/texture/abc"
		);
	}
}
