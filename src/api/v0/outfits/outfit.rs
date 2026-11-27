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
	ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, DatabaseTransaction,
	EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
	sea_query::Expr,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
	OutfitError, OutfitInfo, SkinInfo, lock_player, next_numbered_name,
	normalize_cape_id, validate_name,
};
use crate::api::{
	ApiState,
	state::holding_slot_rules,
	v0::{
		account::AuthenticatedPlayer,
		bonuses::{deliver_discoveries, record_completions, refresh_bonuses},
		websocket::{
			announce_equipment_change, announce_particle_color_change, commit_look,
			commit_particle_color, under_slot_rules,
			colors::{self, ParticleChange},
		},
	},
};

const STARTER_OUTFITS: u64 = 8;
const OUTFIT_NAME_PREFIX: &str = "Outfit #";

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
	blank: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ApplyOutfitResponse {
	outfit: OutfitInfo,
	skin: Option<SkinInfo>,
	adopted: bool,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listOutfits")
		.summary("List the player's outfits")
		.description(
			"Lists the authenticated player's outfits, oldest first, along with the \
			 id of the active outfit and the library limit. The first list creates \
			 eight blank outfits, `Outfit #1` to `Outfit #8`; `Outfit #1` holds the \
			 player's current cosmetics and particle color and becomes the active \
			 outfit, so the client can save the current skin and Mojang cape into \
			 it.",
		)
		.tag("outfits")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createOutfit")
		.summary("Create an outfit from the current look")
		.description(
			"Creates an outfit holding the player's currently equipped cosmetics plus \
			 the given skin, Mojang cape and particle color, and makes it the active \
			 outfit. The particle color defaults to the player's current one; a \
			 different color is applied to the player and broadcast. Names default \
			 to the lowest unused `Outfit #N`.",
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
			 `null` clears `skin_id`, `mojang_cape_id` or `particle_color`. `blank: \
			 false` marks the outfit as no longer blank, even when `skin_id` and \
			 `mojang_cape_id` are `null`; clients send it with the `skin_id` and \
			 `mojang_cape_id` they save into an adopted outfit. `blank: true` is \
			 rejected. Without `blank`, setting `skin_id` or `mojang_cape_id` to a \
			 value marks the outfit as no longer blank. Changing the active outfit's \
			 `particle_color` also applies it to the player and broadcasts it.",
		)
		.tag("outfits")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"The skin is not in the player's library, `blank` is `true`, or a \
				 field is invalid",
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
			 connections. The client applies the outfit's skin and Mojang cape itself. \
			 A blank outfit instead takes on the player's current cosmetics, and its \
			 own particle color or else the current one, and becomes active without \
			 changing what is equipped. It stays blank: the response has `adopted` \
			 set to `true`, the outfit's `blank` still `true` and its `skin_id` and \
			 `mojang_cape_id` unset. The client then saves the player's current skin \
			 and Mojang cape into the outfit with an update that also sends `blank: \
			 false`, instead of changing them.",
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

async fn seed_starter_outfits(
	state: &ApiState,
	player: &user::Model,
) -> Result<Option<i32>, OutfitError> {
	let txn = state.database.begin().await?;

	let claimed = User::update_many()
		.col_expr(user::Column::OutfitsSeeded, Expr::value(true))
		.filter(user::Column::Id.eq(player.id))
		.filter(user::Column::OutfitsSeeded.eq(false))
		.exec(&txn)
		.await?
		.rows_affected
		== 1;
	if !claimed {
		return Ok(None);
	}

	let existing = PlayerOutfit::find()
		.filter(player_outfit::Column::PlayerId.eq(player.id))
		.count(&txn)
		.await?;
	if existing > 0 {
		txn.commit().await?;
		return Ok(None);
	}

	let player = lock_player(&txn, player).await?;
	let (equipped, particle_color) = current_look(state, &txn, &player).await?;
	let equipped = wearable_look(&txn, equipped).await?;

	let starters = STARTER_OUTFITS.min(state.settings.get().outfits.max_outfits);
	let mut first = None;
	for number in 1..=starters {
		let outfit = player_outfit::ActiveModel {
			player_id: Set(player.id),
			name: Set(format!("{OUTFIT_NAME_PREFIX}{number}")),
			favorite: Set(false),
			particle_color: Set(if number == 1 { particle_color } else { None }),
			blank: Set(true),
			..Default::default()
		}
		.insert(&txn)
		.await?;
		first.get_or_insert(outfit.id);
	}
	let Some(first) = first else {
		txn.commit().await?;
		return Ok(None);
	};

	insert_outfit_cosmetics(&txn, first, &equipped).await?;
	activate_outfit(&txn, &player, first, None).await?;

	txn.commit().await?;

	Ok(Some(first))
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

async fn stored_equipment<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
) -> Result<HashMap<BodySlot, i32>, sea_orm::DbErr> {
	Ok(PlayerEquippedCosmetic::find()
		.filter(player_equipped_cosmetic::Column::PlayerId.eq(player_id))
		.all(db)
		.await?
		.into_iter()
		.map(|row| (row.slot, row.cosmetic_id))
		.collect())
}

async fn current_look<C: ConnectionTrait>(
	state: &ApiState,
	db: &C,
	player: &user::Model,
) -> Result<(HashMap<BodySlot, i32>, Option<i32>), OutfitError> {
	if let Some(runtime) = state
		.realtime
		.player_runtime
		.read()
		.await
		.get(&player.minecraft_uuid)
	{
		return Ok((runtime.equipped.clone(), runtime.particle_color));
	}

	Ok((stored_equipment(db, player.id).await?, player.particle_color))
}

async fn hold_slot_rules<C: ConnectionTrait>(
	db: &C,
	look: &HashMap<BodySlot, i32>,
) -> Result<HashSet<(i32, BodySlot)>, sea_orm::DbErr> {
	if look.is_empty() {
		return Ok(HashSet::new());
	}

	holding_slot_rules(look.values().copied()).all(db).await?;
	Ok(CosmeticAllowedSlot::find()
		.filter(cosmetic_allowed_slot::Column::CosmeticId.is_in(look.values().copied()))
		.all(db)
		.await?
		.into_iter()
		.map(|row| (row.cosmetic_id, row.slot))
		.collect())
}

fn outside_slot_rules(
	look: &HashMap<BodySlot, i32>,
	allowed: &HashSet<(i32, BodySlot)>,
) -> Vec<BodySlot> {
	look.iter()
		.filter(|(slot, cosmetic_id)| !allowed.contains(&(**cosmetic_id, (*slot).clone())))
		.map(|(slot, _)| slot.clone())
		.collect()
}

async fn wearable_look<C: ConnectionTrait>(
	db: &C,
	mut look: HashMap<BodySlot, i32>,
) -> Result<HashMap<BodySlot, i32>, sea_orm::DbErr> {
	let allowed = hold_slot_rules(db, &look).await?;
	for slot in outside_slot_rules(&look, &allowed) {
		look.remove(&slot);
	}
	Ok(look)
}

fn outfit_slots_dropped(
	outfit_id: i32,
	slots: Vec<BodySlot>,
) -> sea_orm::DeleteMany<PlayerOutfitCosmetic> {
	PlayerOutfitCosmetic::delete_many()
		.filter(player_outfit_cosmetic::Column::OutfitId.eq(outfit_id))
		.filter(player_outfit_cosmetic::Column::Slot.is_in(slots))
}

async fn insert_outfit_cosmetics<C: ConnectionTrait>(
	db: &C,
	outfit_id: i32,
	equipped: &HashMap<BodySlot, i32>,
) -> Result<(), sea_orm::DbErr> {
	if equipped.is_empty() {
		return Ok(());
	}

	PlayerOutfitCosmetic::insert_many(equipped.iter().map(|(slot, cosmetic_id)| {
		player_outfit_cosmetic::ActiveModel {
			outfit_id: Set(outfit_id),
			slot: Set(slot.clone()),
			cosmetic_id: Set(*cosmetic_id),
		}
	}))
	.exec(db)
	.await?;

	Ok(())
}

async fn find_outfit_skin<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	outfit: &player_outfit::Model,
) -> Result<Option<player_skin::Model>, sea_orm::DbErr> {
	let Some(skin_id) = outfit.skin_id else {
		return Ok(None);
	};

	PlayerSkin::find_by_id(skin_id)
		.filter(player_skin::Column::PlayerId.eq(player_id))
		.one(db)
		.await
}

async fn store_aura_palette(
	db: &DatabaseTransaction,
	player: &user::Model,
	equipped: &HashMap<BodySlot, i32>,
	color: Option<i32>,
) -> Result<Option<ParticleChange>, sea_orm::DbErr> {
	let Some(aura) = equipped.get(&BodySlot::Aura).copied() else {
		return Ok(None);
	};
	let saved = colors::load_palette(db, player.id, aura).await?;
	let planned = colors::plan_particle_color(aura, saved, color);
	colors::store_colors(
		db,
		player.id,
		planned.aura,
		planned.stored.as_ref(),
		true,
		planned.active,
	)
	.await?;
	Ok(Some(planned))
}

async fn override_particle_color(
	state: &ApiState,
	player: Uuid,
	color: Option<i32>,
	planned: Option<&ParticleChange>,
) {
	let mut player_runtime = state.realtime.player_runtime.write().await;
	if let Some(runtime) = player_runtime.get_mut(&player) {
		colors::apply_particle_color(runtime, color, planned);
	}
}

struct ColorOverride {
	color: Option<i32>,
	planned: Option<ParticleChange>,
}

async fn after_commit<T>(
	committed: impl Future<Output = Result<(), sea_orm::DbErr>>,
	apply: impl Future<Output = T>,
) -> Result<T, sea_orm::DbErr> {
	committed.await?;
	Ok(apply.await)
}

async fn commit_color_override(
	state: &ApiState,
	player: Uuid,
	txn: DatabaseTransaction,
	change: Option<ColorOverride>,
) -> Result<(), sea_orm::DbErr> {
	let Some(change) = change else {
		return txn.commit().await;
	};
	after_commit(
		commit_particle_color(state, player, txn),
		override_particle_color(state, player, change.color, change.planned.as_ref()),
	)
	.await
}

async fn activate_outfit<C: ConnectionTrait>(
	db: &C,
	player: &user::Model,
	outfit_id: i32,
	particle_color: Option<Option<i32>>,
) -> Result<(), sea_orm::DbErr> {
	let mut update = User::update_many()
		.col_expr(user::Column::ActiveOutfitId, Expr::value(Some(outfit_id)));
	if let Some(particle_color) = particle_color {
		update = update.col_expr(user::Column::ParticleColor, Expr::value(particle_color));
	}
	update.filter(user::Column::Id.eq(player.id)).exec(db).await?;

	Ok(())
}

fn clears_blank(
	blank: Option<bool>,
	skin_id: Option<Option<i32>>,
	mojang_cape_id: Option<&Option<String>>,
) -> Result<bool, OutfitError> {
	match blank {
		Some(true) => Err(OutfitError::InvalidBlank),
		Some(false) => Ok(true),
		None => Ok(
			matches!(skin_id, Some(Some(_))) || matches!(mojang_cape_id, Some(Some(_)))
		),
	}
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<OutfitListResponse>, OutfitError> {
	let mut active_outfit_id = player.active_outfit_id;
	if !player.outfits_seeded {
		active_outfit_id = match seed_starter_outfits(&state, &player).await? {
			Some(seeded) => Some(seeded),
			None => User::find_by_id(player.id)
				.one(&state.database)
				.await?
				.and_then(|player| player.active_outfit_id),
		};
	}

	let outfits = PlayerOutfit::find()
		.filter(player_outfit::Column::PlayerId.eq(player.id))
		.order_by_asc(player_outfit::Column::CreatedAt)
		.order_by_asc(player_outfit::Column::Id)
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
		active_outfit_id,
		limit: state.settings.get().outfits.max_outfits,
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

	let _updates = state.realtime.player_updates.lock(player.minecraft_uuid).await;
	let txn = state.database.begin().await?;
	let player = lock_player(&txn, &player).await?;

	let existing = PlayerOutfit::find()
		.filter(player_outfit::Column::PlayerId.eq(player.id))
		.all(&txn)
		.await?;
	let max = state.settings.get().outfits.max_outfits;
	if existing.len() as u64 >= max {
		return Err(OutfitError::OutfitLimit(max));
	}

	if let Some(skin_id) = body.skin_id {
		require_skin(&txn, player.id, skin_id).await?;
	}

	let (equipped, current_color) = current_look(&state, &txn, &player).await?;
	let equipped = wearable_look(&txn, equipped).await?;
	let particle_color = body.particle_color.or(current_color);

	let outfit = player_outfit::ActiveModel {
		player_id: Set(player.id),
		name: Set(name.unwrap_or_else(|| {
			next_numbered_name(
				OUTFIT_NAME_PREFIX,
				existing.iter().map(|outfit| outfit.name.as_str()),
			)
		})),
		favorite: Set(false),
		skin_id: Set(body.skin_id),
		mojang_cape_id: Set(mojang_cape_id),
		particle_color: Set(particle_color),
		..Default::default()
	}
	.insert(&txn)
	.await?;

	insert_outfit_cosmetics(&txn, outfit.id, &equipped).await?;

	let color_changed = particle_color != current_color;
	activate_outfit(&txn, &player, outfit.id, color_changed.then_some(particle_color))
		.await?;
	let color_override = if color_changed {
		let planned = store_aura_palette(&txn, &player, &equipped, particle_color).await?;
		Some(ColorOverride {
			color: particle_color,
			planned,
		})
	} else {
		None
	};

	commit_color_override(&state, player.minecraft_uuid, txn, color_override).await?;

	if color_changed {
		announce_particle_color_change(&state, player.minecraft_uuid, particle_color, true)
			.await;
	}

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
	let clears_blank = clears_blank(body.blank, body.skin_id, mojang_cape_id.as_ref())?;

	let _updates = state.realtime.player_updates.lock(player.minecraft_uuid).await;
	let txn = state.database.begin().await?;
	let player = lock_player(&txn, &player).await?;

	let outfit = find_outfit(&txn, player.id, id).await?;
	if let Some(Some(skin_id)) = body.skin_id {
		require_skin(&txn, player.id, skin_id).await?;
	}
	let is_active = player.active_outfit_id == Some(outfit.id);

	let mut active: player_outfit::ActiveModel = outfit.into();
	if let Some(name) = name {
		active.name = Set(name);
	}
	if let Some(favorite) = body.favorite {
		active.favorite = Set(favorite);
	}
	if clears_blank {
		active.blank = Set(false);
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
	let outfit = active.update(&txn).await?;
	let info = outfit_info(&txn, outfit).await?;

	let applied_color = body.particle_color.filter(|_| is_active);
	let mut color_override = None;
	if let Some(particle_color) = applied_color {
		User::update_many()
			.col_expr(user::Column::ParticleColor, Expr::value(particle_color))
			.filter(user::Column::Id.eq(player.id))
			.exec(&txn)
			.await?;
		let (equipped, _) = current_look(&state, &txn, &player).await?;
		let planned = store_aura_palette(&txn, &player, &equipped, particle_color).await?;
		color_override = Some(ColorOverride {
			color: particle_color,
			planned,
		});
	}

	commit_color_override(&state, player.minecraft_uuid, txn, color_override).await?;

	if let Some(particle_color) = applied_color {
		announce_particle_color_change(&state, player.minecraft_uuid, particle_color, true)
			.await;
	}

	Ok(Json(info))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn delete(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<StatusCode, OutfitError> {
	let txn = state.database.begin().await?;
	lock_player(&txn, &player).await?;

	let result = PlayerOutfit::delete_many()
		.filter(player_outfit::Column::Id.eq(id))
		.filter(player_outfit::Column::PlayerId.eq(player.id))
		.exec(&txn)
		.await?;
	if result.rows_affected == 0 {
		return Err(OutfitError::OutfitMissing);
	}

	txn.commit().await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn apply(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(id): Path<i32>,
) -> Result<Json<ApplyOutfitResponse>, OutfitError> {
	under_slot_rules(&state.realtime, apply_outfit(&state, player, id))
		.await
		.map(Json)
}

async fn apply_outfit(
	state: &ApiState,
	player: user::Model,
	id: i32,
) -> Result<ApplyOutfitResponse, OutfitError> {
	let _updates = state.realtime.player_updates.lock(player.minecraft_uuid).await;
	let txn = state.database.begin().await?;
	let player = lock_player(&txn, &player).await?;

	let outfit = find_outfit(&txn, player.id, id).await?;
	if outfit.blank {
		return adopt_current_look(state, txn, &player, outfit).await;
	}

	let mut outfit_cosmetics = load_outfit_cosmetics(&txn, [outfit.id])
		.await?
		.remove(&outfit.id)
		.unwrap_or_default();

	let allowed = hold_slot_rules(&txn, &outfit_cosmetics).await?;
	let removed = outside_slot_rules(&outfit_cosmetics, &allowed);
	let outfit = if removed.is_empty() {
		outfit
	} else {
		for slot in &removed {
			outfit_cosmetics.remove(slot);
		}
		outfit_slots_dropped(outfit.id, removed).exec(&txn).await?;
		let mut active: player_outfit::ActiveModel = outfit.into();
		active.updated_at = Set(chrono::Utc::now().fixed_offset());
		active.update(&txn).await?
	};

	let owned: HashSet<i32> = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player.id))
		.filter(
			player_owned_cosmetic::Column::CosmeticId
				.is_in(outfit_cosmetics.values().copied()),
		)
		.all(&txn)
		.await?
		.into_iter()
		.map(|row| row.cosmetic_id)
		.collect();

	let equipped: HashMap<BodySlot, i32> = outfit_cosmetics
		.iter()
		.filter(|(_, cosmetic_id)| owned.contains(cosmetic_id))
		.map(|(slot, cosmetic_id)| (slot.clone(), *cosmetic_id))
		.collect();

	let stored = stored_equipment(&txn, player.id).await?;
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

	let particle_color = outfit.particle_color;
	activate_outfit(&txn, &player, outfit.id, Some(particle_color)).await?;
	let planned = store_aura_palette(&txn, &player, &equipped, particle_color).await?;
	let skin = find_outfit_skin(&txn, player.id, &outfit).await?;
	let recorded = record_completions(&txn, &state.bonus_catalog, player.id).await?;

	let previous = after_commit(commit_look(state, player.minecraft_uuid, txn), async {
		let mut player_runtime = state.realtime.player_runtime.write().await;
		player_runtime.get_mut(&player.minecraft_uuid).map(|runtime| {
			let previous = std::mem::replace(&mut runtime.equipped, equipped.clone());
			colors::apply_particle_color(runtime, particle_color, planned.as_ref());
			previous
		})
	})
	.await?
	.unwrap_or(stored);

	let slots: HashSet<BodySlot> =
		previous.keys().chain(equipped.keys()).cloned().collect();
	for slot in slots {
		let cosmetic_id = equipped.get(&slot).copied();
		if previous.get(&slot).copied() != cosmetic_id {
			announce_equipment_change(
				state,
				player.minecraft_uuid,
				slot,
				cosmetic_id,
				true,
			)
			.await;
		}
	}
	refresh_bonuses(state, [player.minecraft_uuid]).await;
	if recorded {
		deliver_discoveries(state, player.minecraft_uuid, player.id).await;
	}
	announce_particle_color_change(state, player.minecraft_uuid, particle_color, true)
		.await;

	Ok(ApplyOutfitResponse {
		outfit: OutfitInfo::from_model(outfit, outfit_cosmetics),
		skin: skin.map(SkinInfo::from_model),
		adopted: false,
	})
}

async fn adopt_current_look(
	state: &ApiState,
	txn: DatabaseTransaction,
	player: &user::Model,
	outfit: player_outfit::Model,
) -> Result<ApplyOutfitResponse, OutfitError> {
	let (equipped, current_color) = current_look(state, &txn, player).await?;
	let equipped = wearable_look(&txn, equipped).await?;
	let particle_color = outfit.particle_color.or(current_color);

	PlayerOutfitCosmetic::delete_many()
		.filter(player_outfit_cosmetic::Column::OutfitId.eq(outfit.id))
		.exec(&txn)
		.await?;
	insert_outfit_cosmetics(&txn, outfit.id, &equipped).await?;

	let mut active: player_outfit::ActiveModel = outfit.into();
	active.particle_color = Set(particle_color);
	active.updated_at = Set(chrono::Utc::now().fixed_offset());
	let outfit = active.update(&txn).await?;

	let color_changed = particle_color != current_color;
	activate_outfit(&txn, player, outfit.id, color_changed.then_some(particle_color))
		.await?;
	let skin = find_outfit_skin(&txn, player.id, &outfit).await?;
	let color_override = if color_changed {
		let planned = store_aura_palette(&txn, player, &equipped, particle_color).await?;
		Some(ColorOverride {
			color: particle_color,
			planned,
		})
	} else {
		None
	};

	commit_color_override(state, player.minecraft_uuid, txn, color_override).await?;

	if color_changed {
		announce_particle_color_change(state, player.minecraft_uuid, particle_color, true)
			.await;
	}

	Ok(ApplyOutfitResponse {
		outfit: OutfitInfo::from_model(outfit, equipped),
		skin: skin.map(SkinInfo::from_model),
		adopted: true,
	})
}

#[cfg(test)]
mod tests {
	use std::{
		cell::Cell,
		collections::{HashMap, HashSet},
	};

	use entities::{player_outfit, sea_orm_active_enums::BodySlot};
	use sea_orm::{DbBackend, DbErr, QueryTrait};

	use super::{
		super::{OutfitInfo, next_numbered_name},
		after_commit, outfit_slots_dropped, outside_slot_rules,
	};

	#[test]
	fn outfits_keep_only_allowed_slots_and_carry_effect_slots() {
		let look = HashMap::from([(BodySlot::Hat, 5), (BodySlot::Backpack, 5), (BodySlot::Aura, 7)]);
		let allowed = HashSet::from([(5, BodySlot::Hat), (7, BodySlot::Hat)]);
		let mut outside = outside_slot_rules(&look, &allowed);
		outside.sort_by_key(|slot| format!("{slot:?}"));
		assert_eq!(outside, vec![BodySlot::Aura, BodySlot::Backpack]);

		let sql = outfit_slots_dropped(4, vec![BodySlot::Backpack]).build(DbBackend::Postgres).to_string();
		assert!(sql.starts_with(r#"DELETE FROM "player_outfit_cosmetic""#), "{sql}");
		assert!(sql.contains(r#""player_outfit_cosmetic"."outfit_id" = 4"#), "{sql}");
		assert!(sql.contains(r#""player_outfit_cosmetic"."slot" IN ("#), "{sql}");

		let outfit = player_outfit::Model {
			id: 1,
			player_id: 2,
			name: "Outfit #1".into(),
			favorite: false,
			skin_id: None,
			mojang_cape_id: None,
			particle_color: None,
			blank: false,
			created_at: Default::default(),
			updated_at: Default::default(),
		};
		let equipped = HashMap::from([(BodySlot::EffectA, 7), (BodySlot::EffectB, 8)]);
		let serialized = serde_json::to_value(OutfitInfo::from_model(outfit, equipped)).unwrap();
		assert_eq!(serialized["equipped"], serde_json::json!({ "effect_a": 7, "effect_b": 8 }));
	}

	#[tokio::test]
	async fn the_live_look_changes_only_once_the_commit_lands() {
		let applied = Cell::new(false);
		let failed = after_commit(async { Err(DbErr::Custom("down".to_owned())) }, async { applied.set(true) });
		assert!(failed.await.is_err());
		assert!(!applied.get());
		let landed = after_commit(async { Ok(()) }, async {
			applied.set(true);
			7
		});
		assert_eq!(landed.await.ok(), Some(7));
		assert!(applied.get());
	}

	fn next_outfit_name<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
		next_numbered_name(super::OUTFIT_NAME_PREFIX, names)
	}

	#[test]
	fn names_the_first_outfit() {
		assert_eq!(next_outfit_name([]), "Outfit #1");
	}

	#[test]
	fn fills_the_lowest_gap() {
		assert_eq!(
			next_outfit_name(["Outfit #1", "Outfit #2", "Outfit #4"]),
			"Outfit #3"
		);
	}

	#[test]
	fn ignores_custom_names() {
		assert_eq!(
			next_outfit_name(["Summer", "Outfit #1", "Outfit #x", "Outfit #2"]),
			"Outfit #3"
		);
	}

	#[test]
	fn explicit_blank_false_clears_blank_without_a_look() {
		assert!(super::clears_blank(Some(false), Some(None), Some(&None)).unwrap());
		assert!(super::clears_blank(Some(false), None, None).unwrap());
	}

	#[test]
	fn rejects_marking_an_outfit_blank() {
		assert!(matches!(
			super::clears_blank(Some(true), Some(Some(1)), None),
			Err(super::OutfitError::InvalidBlank)
		));
	}

	#[test]
	fn absent_blank_clears_only_for_a_skin_or_cape() {
		let cape = Some("cape".to_string());
		assert!(super::clears_blank(None, Some(Some(1)), None).unwrap());
		assert!(super::clears_blank(None, None, Some(&cape)).unwrap());
		assert!(!super::clears_blank(None, Some(None), Some(&None)).unwrap());
		assert!(!super::clears_blank(None, None, None).unwrap());
	}
}
