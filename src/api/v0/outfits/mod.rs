mod outfit;
mod skin;

use std::collections::{HashMap, HashSet};

use aide::{OperationIo, axum::ApiRouter};
use axum::{http::StatusCode, response::IntoResponse};
use chrono::{DateTime, FixedOffset};
use entities::{
	player_outfit, player_outfit_cosmetic, player_skin, sea_orm_active_enums::BodySlot,
	user,
};
use schemars::JsonSchema;
use sea_orm::{ConnectionTrait, DbErr};
use serde::{Deserialize, Serialize};

use crate::api::ApiState;

pub(super) const OUTFIT_LIMIT: u64 = 128;
pub(super) const SKIN_LIMIT: u64 = 128;
const MAX_NAME_LENGTH: usize = 64;
const MAX_CAPE_ID_LENGTH: usize = 64;
const MIN_TEXTURE_HASH_LENGTH: usize = 32;
const MAX_TEXTURE_HASH_LENGTH: usize = 64;
const MOJANG_TEXTURE_URL: &str = "https://textures.minecraft.net/texture/";

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(outfit::router())
		.merge(skin::router())
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum OutfitError {
	#[error("No outfit with that id exists")]
	OutfitMissing,
	#[error("No skin with that id exists")]
	SkinMissing,
	#[error("The given skin is not in your skin library")]
	InvalidSkin,
	#[error("You can have at most {} outfits", OUTFIT_LIMIT)]
	OutfitLimit,
	#[error("You can have at most {} skins", SKIN_LIMIT)]
	SkinLimit,
	#[error("Names must be between 1 and {} characters long", MAX_NAME_LENGTH)]
	InvalidName,
	#[error("Mojang cape ids must be at most {} characters long", MAX_CAPE_ID_LENGTH)]
	InvalidCapeId,
	#[error(
		"Texture hashes must be {}-{} hexadecimal characters",
		MIN_TEXTURE_HASH_LENGTH,
		MAX_TEXTURE_HASH_LENGTH
	)]
	InvalidTextureHash,
	#[error("Unable to query database: {0}")]
	Database(#[from] DbErr),
}

impl IntoResponse for OutfitError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match &self {
				Self::OutfitMissing | Self::SkinMissing => StatusCode::NOT_FOUND,
				Self::InvalidSkin
				| Self::InvalidName
				| Self::InvalidCapeId
				| Self::InvalidTextureHash => StatusCode::BAD_REQUEST,
				Self::OutfitLimit | Self::SkinLimit => StatusCode::CONFLICT,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkinModel {
	Classic,
	Slim,
}

impl SkinModel {
	fn as_str(self) -> &'static str {
		match self {
			Self::Classic => "classic",
			Self::Slim => "slim",
		}
	}

	fn parse(value: &str) -> Option<Self> {
		match value {
			"classic" => Some(Self::Classic),
			"slim" => Some(Self::Slim),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SkinInfo {
	id: i32,
	name: String,
	model: SkinModel,
	url: String,
	hash: String,
	favorite: bool,
	created_at: DateTime<FixedOffset>,
}

impl SkinInfo {
	fn from_model(skin: player_skin::Model) -> Self {
		Self {
			id: skin.id,
			name: skin.name,
			model: SkinModel::parse(&skin.model).unwrap_or(SkinModel::Classic),
			url: texture_url(&skin.hash),
			hash: skin.hash,
			favorite: skin.favorite,
			created_at: skin.created_at,
		}
	}
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OutfitInfo {
	id: i32,
	name: String,
	favorite: bool,
	skin_id: Option<i32>,
	mojang_cape_id: Option<String>,
	particle_color: Option<i32>,
	blank: bool,
	equipped: HashMap<BodySlot, i32>,
	created_at: DateTime<FixedOffset>,
	updated_at: DateTime<FixedOffset>,
}

impl OutfitInfo {
	fn from_model(
		outfit: player_outfit::Model,
		equipped: HashMap<BodySlot, i32>,
	) -> Self {
		Self {
			id: outfit.id,
			name: outfit.name,
			favorite: outfit.favorite,
			skin_id: outfit.skin_id,
			mojang_cape_id: outfit.mojang_cape_id,
			particle_color: outfit.particle_color,
			blank: outfit.blank,
			equipped,
			created_at: outfit.created_at,
			updated_at: outfit.updated_at,
		}
	}
}

fn validate_name(name: &str) -> Result<String, OutfitError> {
	let trimmed = name.trim();
	if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME_LENGTH {
		return Err(OutfitError::InvalidName);
	}

	Ok(trimmed.to_string())
}

fn texture_url(hash: &str) -> String {
	format!("{MOJANG_TEXTURE_URL}{hash}")
}

fn normalize_texture_hash(hash: &str) -> Result<String, OutfitError> {
	let hash = hash.trim().to_ascii_lowercase();
	if !(MIN_TEXTURE_HASH_LENGTH..=MAX_TEXTURE_HASH_LENGTH).contains(&hash.len())
		|| !hash.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
	{
		return Err(OutfitError::InvalidTextureHash);
	}

	Ok(hash)
}

fn normalize_cape_id(cape_id: Option<String>) -> Result<Option<String>, OutfitError> {
	let Some(cape_id) = cape_id else {
		return Ok(None);
	};

	let trimmed = cape_id.trim();
	if trimmed.is_empty() {
		return Ok(None);
	}
	if trimmed.chars().count() > MAX_CAPE_ID_LENGTH {
		return Err(OutfitError::InvalidCapeId);
	}

	Ok(Some(trimmed.to_string()))
}

fn next_numbered_name<'a>(
	prefix: &str,
	names: impl IntoIterator<Item = &'a str>,
) -> String {
	let taken: HashSet<u64> = names
		.into_iter()
		.filter_map(|name| name.strip_prefix(prefix)?.parse().ok())
		.collect();
	let number = (1..).find(|number| !taken.contains(number)).unwrap_or(1);
	format!("{prefix}{number}")
}

async fn lock_player<C: ConnectionTrait>(
	db: &C,
	player: &user::Model,
) -> Result<user::Model, DbErr> {
	use entities::prelude::*;
	use sea_orm::{EntityTrait, QuerySelect};

	Ok(User::find_by_id(player.id)
		.lock_exclusive()
		.one(db)
		.await?
		.unwrap_or_else(|| player.clone()))
}

pub(crate) async fn mirror_active_outfit_slots<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	active_outfit_id: Option<i32>,
	changes: impl IntoIterator<Item = (BodySlot, Option<i32>)>,
) -> Result<(), DbErr> {
	use entities::prelude::*;
	use sea_orm::{
		ColumnTrait, EntityTrait, QueryFilter, Set,
		sea_query::{Expr, OnConflict},
	};

	let Some(outfit_id) = active_outfit_id else {
		return Ok(());
	};

	let mut equipped = Vec::new();
	let mut cleared = Vec::new();
	for (slot, cosmetic_id) in changes {
		match cosmetic_id {
			Some(cosmetic_id) => equipped.push(player_outfit_cosmetic::ActiveModel {
				outfit_id: Set(outfit_id),
				slot: Set(slot),
				cosmetic_id: Set(cosmetic_id),
			}),
			None => cleared.push(slot),
		}
	}
	if equipped.is_empty() && cleared.is_empty() {
		return Ok(());
	}

	if !equipped.is_empty() {
		PlayerOutfitCosmetic::insert_many(equipped)
			.on_conflict(
				OnConflict::columns([
					player_outfit_cosmetic::Column::OutfitId,
					player_outfit_cosmetic::Column::Slot,
				])
				.update_column(player_outfit_cosmetic::Column::CosmeticId)
				.to_owned(),
			)
			.exec(db)
			.await?;
	}
	if !cleared.is_empty() {
		PlayerOutfitCosmetic::delete_many()
			.filter(player_outfit_cosmetic::Column::OutfitId.eq(outfit_id))
			.filter(player_outfit_cosmetic::Column::Slot.is_in(cleared))
			.exec(db)
			.await?;
	}

	PlayerOutfit::update_many()
		.col_expr(player_outfit::Column::UpdatedAt, Expr::current_timestamp().into())
		.filter(player_outfit::Column::Id.eq(outfit_id))
		.filter(player_outfit::Column::PlayerId.eq(player_id))
		.exec(db)
		.await?;

	Ok(())
}

pub(crate) async fn mirror_active_outfit_particle_color<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	active_outfit_id: Option<i32>,
	color: Option<i32>,
) -> Result<(), DbErr> {
	use entities::prelude::*;
	use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

	let Some(outfit_id) = active_outfit_id else {
		return Ok(());
	};

	PlayerOutfit::update_many()
		.col_expr(player_outfit::Column::ParticleColor, Expr::value(color))
		.col_expr(player_outfit::Column::UpdatedAt, Expr::current_timestamp().into())
		.filter(player_outfit::Column::Id.eq(outfit_id))
		.filter(player_outfit::Column::PlayerId.eq(player_id))
		.exec(db)
		.await?;

	Ok(())
}
