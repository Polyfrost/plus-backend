use std::collections::{BTreeSet, HashMap};

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation
};
use axum::{
	Json,
	extract::{Path, Query, State},
	http::StatusCode,
	response::IntoResponse
};
use chrono::{DateTime, FixedOffset};
use entities::{
	player_equipped_cosmetic,
	player_pet_preferences,
	player_set_discovery,
	prelude::*,
	sea_orm_active_enums::{BodySlot, SharedEmotePrivacy},
	user
};
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, sea_query::Expr};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	permissions::{Require, perm},
	state::PlayerRuntimeState,
	v0::{
		bonuses::BonusRef,
		websocket::colors::{SlotColors, StoredColors, load_colors}
	}
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum PlayerCosmeticsError {
	#[error("No player with that UUID has signed in")]
	UnknownPlayer,
	#[error("No player with that name has signed in")]
	UnknownName,
	#[error("Enter a username")]
	MissingName,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr)
}

impl IntoResponse for PlayerCosmeticsError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::UnknownPlayer | Self::UnknownName => StatusCode::NOT_FOUND,
				Self::MissingName => StatusCode::BAD_REQUEST,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR
			},
			self
		)
	}
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/admin/players/lookup",
			get_with(self::lookup_player, self::lookup_player_doc)
		)
		.api_route(
			"/admin/players/{uuid}/cosmetics",
			get_with(self::player_cosmetics, self::player_cosmetics_doc)
		)
}

fn lookup_player_doc(op: TransformOperation) -> TransformOperation {
	op.id("adminLookupPlayer")
		.summary("Find a signed-in player by username")
		.description(
			"Resolves a case-insensitive exact username to the UUID of a player 			 the backend already knows. Read only: it never asks Mojang and never 			 creates a player. When several players have held the name, the 			 newest one wins. Requires `MANAGE_COSMETICS`."
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description("The name is empty")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No player with that name has signed in")
		})
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LookupQuery {
	name: String
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct PlayerLookup {
	uuid: Uuid,
	username: String
}

fn lookup_name(raw: &str) -> Result<&str, PlayerCosmeticsError> {
	let name = raw.trim();
	if name.is_empty() {
		return Err(PlayerCosmeticsError::MissingName);
	}
	Ok(name)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn lookup_player(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Query(query): Query<LookupQuery>
) -> Result<Json<PlayerLookup>, PlayerCosmeticsError> {
	let name = lookup_name(&query.name)?;
	let player = User::find()
		.filter(user::Column::Username.is_not_null())
		.filter(Expr::cust_with_values("lower(username) = lower($1)", [name]))
		.order_by_desc(user::Column::Id)
		.one(&state.database)
		.await?
		.ok_or(PlayerCosmeticsError::UnknownName)?;
	Ok(Json(PlayerLookup {
		uuid: player.minecraft_uuid,
		username: player.username.unwrap_or_else(|| name.to_owned())
	}))
}

fn player_cosmetics_doc(op: TransformOperation) -> TransformOperation {
	op.id("adminPlayerCosmetics")
		.summary("Show a player's cosmetic state")
		.description(
			"What a player has equipped, the sets they have discovered, their \
			 custom colours, pet preferences and shared emote privacy. `live` is \
			 only present while the player is connected and lists the set bonuses \
			 they are wearing and the emote they are playing. Requires \
			 `MANAGE_COSMETICS`."
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No player with that UUID has signed in")
		})
}

#[derive(Debug, Serialize, JsonSchema)]
struct PlayerCosmeticsResponse {
	uuid: Uuid,
	username: Option<String>,
	equipped: HashMap<BodySlot, i32>,
	particle_color: Option<i32>,
	shared_emote_privacy: SharedEmotePrivacy,
	pet_preferences: PetPreferences,
	discoveries: Vec<Discovery>,
	colors: Vec<CustomColors>,
	live: Option<LiveState>
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct PetPreferences {
	interactions: bool,
	updated_at: Option<DateTime<FixedOffset>>
}

#[derive(Debug, Serialize, JsonSchema)]
struct Discovery {
	set_id: i32,
	name: String,
	cosmetic_ids: Vec<i32>,
	discovered_at: DateTime<FixedOffset>,
	delivered_at: Option<DateTime<FixedOffset>>
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct CustomColors {
	cosmetic_id: i32,
	active: bool,
	palette: SlotColors,
	aura_default: bool
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct LiveState {
	bonuses: Vec<BonusRef>,
	active_emote_id: Option<i32>
}

fn custom_colors(stored: StoredColors) -> Vec<CustomColors> {
	let StoredColors {
		mut palettes,
		aura_defaults,
		mut saved
	} = stored;
	let ids = palettes
		.keys()
		.chain(saved.keys())
		.chain(aura_defaults.iter())
		.copied()
		.collect::<BTreeSet<_>>();
	ids.into_iter()
		.map(|cosmetic_id| {
			let (active, palette) = match palettes.remove(&cosmetic_id) {
				Some(palette) => (true, palette),
				None => (false, saved.remove(&cosmetic_id).unwrap_or_default())
			};
			CustomColors {
				cosmetic_id,
				active,
				palette,
				aura_default: aura_defaults.contains(&cosmetic_id)
			}
		})
		.collect()
}

fn pet_preferences(stored: Option<player_pet_preferences::Model>) -> PetPreferences {
	match stored {
		Some(row) => PetPreferences {
			interactions: row.interactions,
			updated_at: Some(row.updated_at)
		},
		None => PetPreferences {
			interactions: true,
			updated_at: None
		}
	}
}

fn live_state(runtime: Option<&PlayerRuntimeState>) -> Option<LiveState> {
	runtime.filter(|runtime| runtime.live).map(|runtime| LiveState {
		bonuses: runtime.bonuses.clone(),
		active_emote_id: runtime.active_emote.map(|emote| emote.emote_id)
	})
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn player_cosmetics(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Path(uuid): Path<Uuid>
) -> Result<Json<PlayerCosmeticsResponse>, PlayerCosmeticsError> {
	let db = &state.database;
	let player = User::find()
		.filter(user::Column::MinecraftUuid.eq(uuid))
		.one(db)
		.await?
		.ok_or(PlayerCosmeticsError::UnknownPlayer)?;

	let equipped = PlayerEquippedCosmetic::find()
		.filter(player_equipped_cosmetic::Column::PlayerId.eq(player.id))
		.find_also_related(Cosmetic)
		.all(db)
		.await?
		.into_iter()
		.filter_map(|(row, cosmetic)| cosmetic.map(|_| (row.slot, row.cosmetic_id)))
		.collect();

	let discoveries = PlayerSetDiscovery::find()
		.filter(player_set_discovery::Column::PlayerId.eq(player.id))
		.find_also_related(CosmeticSet)
		.order_by_asc(player_set_discovery::Column::DiscoveredAt)
		.order_by_asc(player_set_discovery::Column::SetId)
		.all(db)
		.await?
		.into_iter()
		.filter_map(|(row, set)| {
			Some(Discovery {
				name: set?.name,
				set_id: row.set_id,
				cosmetic_ids: row.cosmetic_ids,
				discovered_at: row.discovered_at,
				delivered_at: row.delivered_at
			})
		})
		.collect();

	let colors = custom_colors(load_colors(db, player.id).await?);
	let pets = pet_preferences(
		PlayerPetPreferences::find_by_id(player.id)
			.one(db)
			.await?
	);
	let live = live_state(state.realtime.player_runtime.read().await.get(&uuid));

	Ok(Json(PlayerCosmeticsResponse {
		uuid,
		username: player.username,
		equipped,
		particle_color: player.particle_color.map(|c| c & 0x00FF_FFFF),
		shared_emote_privacy: player.shared_emote_privacy,
		pet_preferences: pets,
		discoveries,
		colors,
		live
	}))
}

#[cfg(test)]
mod tests {
	use std::collections::{BTreeMap, HashMap, HashSet};

	use axum::{
		extract::Query,
		http::{StatusCode, Uri},
		response::IntoResponse
	};
	use chrono::Utc;
	use entities::player_pet_preferences;
	use serde_json::json;

	use super::{
		CustomColors, LiveState, LookupQuery, PetPreferences, PlayerCosmeticsError,
		PlayerLookup, custom_colors, live_state, lookup_name, pet_preferences
	};
	use crate::api::{
		state::{ActiveEmote, PlayerRuntimeState},
		v0::{bonuses::BonusRef, websocket::colors::StoredColors}
	};

	#[test]
	fn active_palettes_win_over_saved_ones_and_serialise_as_slot_maps() {
		let palette = |slot: &str, color| BTreeMap::from([(slot.to_owned(), color)]);
		let stored = StoredColors {
			palettes: HashMap::from([(1, palette("trim", 0xff0000))]),
			aura_defaults: HashSet::from([3]),
			saved: HashMap::from([(1, palette("trim", 0x00ff00)), (2, palette("glow", 0x0000ff))])
		};
		let entry = |cosmetic_id, active, palette, aura_default| CustomColors { cosmetic_id, active, palette, aura_default };
		assert_eq!(custom_colors(stored), vec![
			entry(1, true, palette("trim", 0xff0000), false),
			entry(2, false, palette("glow", 0x0000ff), false),
			entry(3, false, BTreeMap::new(), true)
		]);
		assert_eq!(
			serde_json::to_value(entry(7, true, palette("ink", 0x123456), false)).unwrap(),
			json!({ "cosmetic_id": 7, "active": true, "palette": { "ink": 0x123456 }, "aura_default": false })
		);
	}

	#[test]
	fn lookups_take_a_trimmed_name_from_the_query() {
		assert_eq!(lookup_name("  Steve ").ok(), Some("Steve"));
		assert_eq!(lookup_name("a_b%").ok(), Some("a_b%"));
		for blank in ["", " \t"] {
			assert!(matches!(lookup_name(blank), Err(PlayerCosmeticsError::MissingName)));
		}
		let uri: Uri = "/admin/players/lookup?name=Steve%20".parse().unwrap();
		assert_eq!(Query::<LookupQuery>::try_from_uri(&uri).unwrap().0.name, "Steve ");
		assert!(Query::<LookupQuery>::try_from_uri(&"/admin/players/lookup".parse().unwrap()).is_err());
		assert_eq!(PlayerCosmeticsError::UnknownName.into_response().status(), StatusCode::NOT_FOUND);
		assert_eq!(PlayerCosmeticsError::MissingName.into_response().status(), StatusCode::BAD_REQUEST);

		let uuid = uuid::Uuid::parse_str("f7c77d99-9f15-4a66-a87d-c4a51ef30d19").unwrap();
		assert_eq!(
			serde_json::to_value(PlayerLookup { uuid, username: "Steve".to_owned() }).unwrap(),
			json!({ "uuid": "f7c77d99-9f15-4a66-a87d-c4a51ef30d19", "username": "Steve" })
		);
	}

	#[test]
	fn pet_preferences_default_on_and_live_state_shows_only_while_connected() {
		assert_eq!(pet_preferences(None), PetPreferences { interactions: true, updated_at: None });
		let now = Utc::now().fixed_offset();
		let row = player_pet_preferences::Model { player_id: 1, interactions: false, play: None, updated_at: now };
		assert_eq!(pet_preferences(Some(row)), PetPreferences { interactions: false, updated_at: Some(now) });

		let bonus = BonusRef { id: 4, url: None, hash: "abc".to_owned() };
		let active_emote = Some(ActiveEmote { emote_id: 9, started_at: 0, anchor: None, shared: None, generation: 1 });
		let mut runtime = PlayerRuntimeState { bonuses: vec![bonus.clone()], active_emote, ..Default::default() };
		assert_eq!(live_state(None), None);
		assert_eq!(live_state(Some(&runtime)), None);
		runtime.live = true;
		assert_eq!(live_state(Some(&runtime)), Some(LiveState { bonuses: vec![bonus], active_emote_id: Some(9) }));
	}
}
