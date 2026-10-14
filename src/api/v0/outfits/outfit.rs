use std::collections::{HashMap, HashSet};

use aide::{
	axum::{
		ApiRouter,
		routing::{get_with, patch_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use entities::{
	cosmetic_allowed_slot, player_equipped_cosmetic, player_outfit,
	player_outfit_cosmetic, player_owned_cosmetic, player_skin, prelude::*,
	sea_orm_active_enums::BodySlot, user,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, EntityTrait,
	PaginatorTrait, QueryFilter, QueryOrder, Set, TransactionTrait, sea_query::Expr,
};
use serde::{Deserialize, Serialize};

use super::{
	OUTFIT_LIMIT, OutfitError, OutfitInfo, SkinInfo, normalize_cape_id, validate_name,
};
use crate::api::{
	ApiState,
	v0::{
		account::AuthenticatedPlayer,
		websocket::{publish_equipment_change, publish_particle_color_change},
	},
};

#[derive(Debug, Serialize, JsonSchema)]
struct OutfitListResponse {
	outfits: Vec<OutfitInfo>,
	active_outfit_id: Option<i32>,
	limit: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateOutfitRequest {
	name: Option<String>,
	skin_id: Option<i32>,
	mojang_cape_id: Option<String>,
	particle_color: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateOutfitRequest {
	name: Option<String>,
	favorite: Option<bool>,
	#[serde(default, deserialize_with = "serde_with::rust::double_option::deserialize")]
	skin_id: Option<Option<i32>>,
	#[serde(default, deserialize_with = "serde_with::rust::double_option::deserialize")]
	mojang_cape_id: Option<Option<String>>,
	#[serde(default, deserialize_with = "serde_with::rust::double_option::deserialize")]
	particle_color: Option<Option<i32>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ApplyOutfitResponse {
	outfit: OutfitInfo,
	skin: Option<SkinInfo>,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listOutfits")
		.summary("List the player's outfits")
		.description(
			"Lists the authenticated player's outfits, newest first, along with the \
			 id of the active outfit and the library limit.",
		)
		.tag("outfits")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createOutfit")
		.summary("Create an outfit from the current look")
		.description(
			"Creates an outfit holding the player's currently equipped cosmetics plus \
			 the given skin, Mojang cape and particle color, and makes it the active \
			 outfit. Names default to `Outfit #N`.",
		)
		.tag("outfits")
		.response_with::<{ StatusCode::CREATED.as_u16() }, Json<OutfitInfo>, _>(|res| {
			res.description("The created outfit")
		})
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"The skin is not in the player's library, or a field is invalid",
			)
		})
		.response_with::<{ StatusCode::CONFLICT.as_u16() }, String, _>(|res| {
			res.description("The player already has the maximum number of outfits")
		})
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateOutfit")
		.summary("Update an outfit")
		.description(
			"Updates an outfit's fields. Absent fields are left unchanged; an explicit \
			 `null` clears `skin_id`, `mojang_cape_id` or `particle_color`.",
		)
		.tag("outfits")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"The skin is not in the player's library, or a field is invalid",
			)
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("The outfit does not exist or is not the player's")
		})
}

fn delete_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteOutfit")
		.summary("Delete an outfit")
		.description(
			"Deletes an outfit. Deleting the active outfit leaves no outfit active.",
		)
		.tag("outfits")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The outfit was deleted")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("The outfit does not exist or is not the player's")
		})
}

fn apply_doc(op: TransformOperation) -> TransformOperation {
	op.id("applyOutfit")
		.summary("Switch to an outfit")
		.description(
			"Replaces the player's equipped cosmetics with the outfit's, skipping any \
			 that are no longer owned, sets the particle color and makes the outfit \
			 active. Changes are broadcast to websocket watchers and the player's own \
			 connections. The client applies the outfit's skin and Mojang cape itself.",
		)
		.tag("outfits")
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("The outfit does not exist or is not the player's")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/outfits",
			get_with(self::list, self::list_doc)
				.post_with(self::create, self::create_doc),
		)
		.api_route(
			"/outfits/{id}",
			patch_with(self::update, self::update_doc)
				.delete_with(self::delete, self::delete_doc),
		)
		.api_route("/outfits/{id}/apply", post_with(self::apply, self::apply_doc))
}

async fn find_outfit<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	id: i32,
) -> Result<player_outfit::Model, OutfitError> {
	PlayerOutfit::find_by_id(id)
		.filter(player_outfit::Column::PlayerId.eq(player_id))
		.one(db)
		.await?
		.ok_or(OutfitError::OutfitMissing)
}

async fn require_skin<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	skin_id: i32,
) -> Result<player_skin::Model, OutfitError> {
	PlayerSkin::find_by_id(skin_id)
		.filter(player_skin::Column::PlayerId.eq(player_id))
		.one(db)
		.await?
		.ok_or(OutfitError::InvalidSkin)
}

async fn load_outfit_cosmetics<C: ConnectionTrait>(
	db: &C,
	outfit_ids: impl IntoIterator<Item = i32>,
) -> Result<HashMap<i32, HashMap<BodySlot, i32>>, sea_orm::DbErr> {
	let mut by_outfit: HashMap<i32, HashMap<BodySlot, i32>> = HashMap::new();
	for row in PlayerOutfitCosmetic::find()
		.filter(player_outfit_cosmetic::Column::OutfitId.is_in(outfit_ids))
		.all(db)
		.await?
	{
		by_outfit
			.entry(row.outfit_id)
			.or_default()
			.insert(row.slot, row.cosmetic_id);
	}

	Ok(by_outfit)
}

async fn outfit_info<C: ConnectionTrait>(
	db: &C,
	outfit: player_outfit::Model,
) -> Result<OutfitInfo, OutfitError> {
	let equipped = load_outfit_cosmetics(db, [outfit.id])
		.await?
		.remove(&outfit.id)
		.unwrap_or_default();

	Ok(OutfitInfo::from_model(outfit, equipped))
}

async fn current_equipment(
	state: &ApiState,
	player: &user::Model,
) -> Result<HashMap<BodySlot, i32>, OutfitError> {
	if let Some(runtime) = state
		.realtime
		.player_runtime
		.read()
		.await
		.get(&player.minecraft_uuid)
	{
		return Ok(runtime.equipped.clone());
	}

	Ok(PlayerEquippedCosmetic::find()
		.filter(player_equipped_cosmetic::Column::PlayerId.eq(player.id))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|row| (row.slot, row.cosmetic_id))
		.collect())
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<OutfitListResponse>, OutfitError> {
	let outfits = PlayerOutfit::find()
		.filter(player_outfit::Column::PlayerId.eq(player.id))
		.order_by_desc(player_outfit::Column::CreatedAt)
		.order_by_desc(player_outfit::Column::Id)
		.all(&state.database)
		.await?;
	let mut cosmetics = load_outfit_cosmetics(
		&state.database,
		outfits.iter().map(|outfit| outfit.id),
	)
	.await?;

	Ok(Json(OutfitListResponse {
		outfits: outfits
			.into_iter()
			.map(|outfit| {
				let equipped = cosmetics.remove(&outfit.id).unwrap_or_default();
				OutfitInfo::from_model(outfit, equipped)
			})
			.collect(),
		active_outfit_id: player.active_outfit_id,
		limit: OUTFIT_LIMIT,
	}))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn create(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<CreateOutfitRequest>,
) -> Result<(StatusCode, Json<OutfitInfo>), OutfitError> {
	let name = body.name.as_deref().map(validate_name).transpose()?;
	let mojang_cape_id = normalize_cape_id(body.mojang_cape_id)?;
	let equipped = current_equipment(&state, &player).await?;

	let txn = state.database.begin().await?;

	let count = PlayerOutfit::find()
		.filter(player_outfit::Column::PlayerId.eq(player.id))
		.count(&txn)
		.await?;
	if count >= OUTFIT_LIMIT {
		return Err(OutfitError::OutfitLimit);
	}

	if let Some(skin_id) = body.skin_id {
		require_skin(&txn, player.id, skin_id).await?;
	}

	let outfit = player_outfit::ActiveModel {
		player_id: Set(player.id),
		name: Set(name.unwrap_or_else(|| format!("Outfit #{}", count + 1))),
		favorite: Set(false),
		skin_id: Set(body.skin_id),
		mojang_cape_id: Set(mojang_cape_id),
		particle_color: Set(body.particle_color),
		..Default::default()
	}
	.insert(&txn)
	.await?;

	if !equipped.is_empty() {
		PlayerOutfitCosmetic::insert_many(equipped.iter().map(|(slot, cosmetic_id)| {
			player_outfit_cosmetic::ActiveModel {
				outfit_id: Set(outfit.id),
				slot: Set(slot.clone()),
				cosmetic_id: Set(*cosmetic_id),
			}
		}))
		.exec(&txn)
		.await?;
	}

	User::update_many()
		.col_expr(user::Column::ActiveOutfitId, Expr::value(Some(outfit.id)))
		.filter(user::Column::Id.eq(player.id))
		.exec(&txn)
		.await?;

	txn.commit().await?;

	Ok((StatusCode::CREATED, Json(OutfitInfo::from_model(outfit, equipped))))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn update(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
	Json(body): Json<UpdateOutfitRequest>,
) -> Result<Json<OutfitInfo>, OutfitError> {
	let name = body.name.as_deref().map(validate_name).transpose()?;
	let mojang_cape_id = body.mojang_cape_id.map(normalize_cape_id).transpose()?;

	let outfit = find_outfit(&state.database, player.id, id).await?;
	if let Some(Some(skin_id)) = body.skin_id {
		require_skin(&state.database, player.id, skin_id).await?;
	}

	let mut active: player_outfit::ActiveModel = outfit.into();
	if let Some(name) = name {
		active.name = Set(name);
	}
	if let Some(favorite) = body.favorite {
		active.favorite = Set(favorite);
	}
	if let Some(skin_id) = body.skin_id {
		active.skin_id = Set(skin_id);
	}
	if let Some(mojang_cape_id) = mojang_cape_id {
		active.mojang_cape_id = Set(mojang_cape_id);
	}
	if let Some(particle_color) = body.particle_color {
		active.particle_color = Set(particle_color);
	}
	active.updated_at = Set(chrono::Utc::now().fixed_offset());
	let outfit = active.update(&state.database).await?;

	Ok(Json(outfit_info(&state.database, outfit).await?))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn delete(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, OutfitError> {
	let result = PlayerOutfit::delete_many()
		.filter(player_outfit::Column::Id.eq(id))
		.filter(player_outfit::Column::PlayerId.eq(player.id))
		.exec(&state.database)
		.await?;
	if result.rows_affected == 0 {
		return Err(OutfitError::OutfitMissing);
	}

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn apply(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<Json<ApplyOutfitResponse>, OutfitError> {
	let previous = current_equipment(&state, &player).await?;

	let txn = state.database.begin().await?;

	let outfit = find_outfit(&txn, player.id, id).await?;
	let outfit_cosmetics = load_outfit_cosmetics(&txn, [outfit.id])
		.await?
		.remove(&outfit.id)
		.unwrap_or_default();

	let cosmetic_ids: Vec<i32> = outfit_cosmetics.values().copied().collect();
	let owned: HashSet<i32> = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player.id))
		.filter(player_owned_cosmetic::Column::CosmeticId.is_in(cosmetic_ids.clone()))
		.all(&txn)
		.await?
		.into_iter()
		.map(|row| row.cosmetic_id)
		.collect();
	let allowed: HashSet<(i32, BodySlot)> = CosmeticAllowedSlot::find()
		.filter(cosmetic_allowed_slot::Column::CosmeticId.is_in(cosmetic_ids))
		.all(&txn)
		.await?
		.into_iter()
		.map(|row| (row.cosmetic_id, row.slot))
		.collect();

	let equipped: HashMap<BodySlot, i32> = outfit_cosmetics
		.iter()
		.filter(|(slot, cosmetic_id)| {
			owned.contains(cosmetic_id)
				&& allowed.contains(&(**cosmetic_id, (*slot).clone()))
		})
		.map(|(slot, cosmetic_id)| (slot.clone(), *cosmetic_id))
		.collect();

	PlayerEquippedCosmetic::delete_many()
		.filter(player_equipped_cosmetic::Column::PlayerId.eq(player.id))
		.exec(&txn)
		.await?;
	if !equipped.is_empty() {
		PlayerEquippedCosmetic::insert_many(equipped.iter().map(|(slot, cosmetic_id)| {
			player_equipped_cosmetic::ActiveModel {
				player_id: Set(player.id),
				slot: Set(slot.clone()),
				cosmetic_id: Set(*cosmetic_id),
				updated_at: ActiveValue::NotSet,
			}
		}))
		.exec(&txn)
		.await?;
	}

	User::update_many()
		.col_expr(user::Column::ParticleColor, Expr::value(outfit.particle_color))
		.col_expr(user::Column::ActiveOutfitId, Expr::value(Some(outfit.id)))
		.filter(user::Column::Id.eq(player.id))
		.exec(&txn)
		.await?;

	let skin = match outfit.skin_id {
		Some(skin_id) => {
			PlayerSkin::find_by_id(skin_id)
				.filter(player_skin::Column::PlayerId.eq(player.id))
				.one(&txn)
				.await?
		}
		None => None,
	};

	txn.commit().await?;

	let slots: HashSet<BodySlot> =
		previous.keys().chain(equipped.keys()).cloned().collect();
	for slot in slots {
		let cosmetic_id = equipped.get(&slot).copied();
		if previous.get(&slot).copied() != cosmetic_id {
			publish_equipment_change(&state, player.minecraft_uuid, slot, cosmetic_id)
				.await;
		}
	}
	publish_particle_color_change(&state, player.minecraft_uuid, outfit.particle_color)
		.await;

	Ok(Json(ApplyOutfitResponse {
		outfit: OutfitInfo::from_model(outfit, outfit_cosmetics),
		skin: skin.map(SkinInfo::from_model),
	}))
}
