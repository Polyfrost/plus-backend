use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::Utc;
use entities::{
	asset_color_slot, cosmetic, player_cosmetic_colors, player_owned_cosmetic,
	prelude::*,
	sea_orm_active_enums::{BodySlot, CosmeticType},
	user,
};
use sea_orm::{
	ColumnTrait as _, ConnectionTrait, DbErr, EntityTrait as _, QueryFilter as _,
	QuerySelect as _, RelationTrait as _, Set, TransactionTrait, sea_query::JoinType,
};
use uuid::Uuid;

use super::structs::WebsocketError;
use crate::api::{
	ApiState,
	state::PlayerRuntimeState,
	v0::cosmetics::color_slots::scan_stored_slots,
};

pub type SlotColors = BTreeMap<String, i32>;

pub(crate) const AURA_SLOT: &str = "aura";
pub(crate) const MAX_COLOR_SLOTS: usize = 32;
const SLOT_ID_MAX_LEN: usize = 64;
const RGB_MASK: i32 = 0x00FF_FFFF;
const OPAQUE: i32 = 0xFF00_0000_u32 as i32;

pub(crate) fn custom_colors_allowed(
	_player: &user::Model,
	_cosmetic: &cosmetic::Model,
) -> bool {
	true
}

pub(crate) fn is_slot_id(slot: &str) -> bool {
	(1..=SLOT_ID_MAX_LEN).contains(&slot.len())
		&& slot
			.bytes()
			.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub(crate) fn normalize(colors: Option<SlotColors>) -> Option<SlotColors> {
	colors.filter(|colors| !colors.is_empty())
}

pub(crate) fn allowed_slots(
	kind: &CosmeticType,
	declared: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
	let mut allowed = declared.into_iter().collect::<BTreeSet<_>>();
	if *kind == CosmeticType::Aura {
		allowed.insert(AURA_SLOT.to_owned());
	}
	allowed
}

pub(crate) fn validate_colors(
	colors: &SlotColors,
	allowed: &BTreeSet<String>,
) -> Result<(), WebsocketError> {
	if colors.len() > MAX_COLOR_SLOTS || colors.len() > allowed.len() {
		return Err(WebsocketError::InvalidColors("too many colour slots"));
	}
	for (slot, color) in colors {
		if !is_slot_id(slot) {
			return Err(WebsocketError::InvalidColors("invalid slot id"));
		}
		if !allowed.contains(slot) {
			return Err(WebsocketError::InvalidColors("slot is not customisable"));
		}
		if !(0..=RGB_MASK).contains(color) {
			return Err(WebsocketError::InvalidColors("colours must be RGB"));
		}
	}
	Ok(())
}

pub(crate) fn aura_colors(particle_color: Option<i32>) -> Option<SlotColors> {
	particle_color
		.map(|color| SlotColors::from([(AURA_SLOT.to_owned(), color & RGB_MASK)]))
}

pub(crate) fn particle_color_from(colors: Option<&SlotColors>) -> Option<i32> {
	colors?.get(AURA_SLOT).map(|rgb| (rgb & RGB_MASK) | OPAQUE)
}

pub(crate) fn merge_aura(
	stored: Option<&SlotColors>,
	particle_color: Option<i32>,
) -> Option<SlotColors> {
	let mut merged = stored.cloned().unwrap_or_default();
	merged.remove(AURA_SLOT);
	merged.extend(aura_colors(particle_color).unwrap_or_default());
	normalize(Some(merged))
}

pub(crate) fn worn_colors(
	slot: &BodySlot,
	cosmetic_id: i32,
	stored: &HashMap<i32, SlotColors>,
	particle_color: Option<i32>,
) -> Option<SlotColors> {
	if *slot == BodySlot::Aura {
		merge_aura(stored.get(&cosmetic_id), particle_color)
	} else {
		stored.get(&cosmetic_id).cloned()
	}
}

pub(crate) fn snapshot_colors(
	equipped: &HashMap<BodySlot, i32>,
	stored: &HashMap<i32, SlotColors>,
	particle_color: Option<i32>,
) -> HashMap<i32, SlotColors> {
	equipped
		.iter()
		.filter_map(|(slot, &cosmetic_id)| {
			worn_colors(slot, cosmetic_id, stored, particle_color)
				.map(|colors| (cosmetic_id, colors))
		})
		.collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColorChange {
	pub is_aura: bool,
	pub particle_color: Option<i32>,
	pub stored: Option<SlotColors>,
	pub active: bool,
}

impl ColorChange {
	pub(crate) fn worn(&self) -> Option<&SlotColors> {
		self.stored.as_ref().filter(|_| self.active)
	}

	pub(crate) fn announced(&self) -> Option<SlotColors> {
		self.worn().cloned()
	}
}

pub(crate) fn plan_change(
	cosmetic_id: i32,
	owned: Option<&cosmetic::Model>,
	entitled: impl FnOnce(&cosmetic::Model) -> bool,
	requested: Option<SlotColors>,
	active: bool,
	declared: impl IntoIterator<Item = String>,
) -> Result<ColorChange, WebsocketError> {
	let cosmetic = owned.ok_or(WebsocketError::UnownedCosmetic(cosmetic_id))?;
	if !entitled(cosmetic) {
		return Err(WebsocketError::CustomColorsNotAllowed(cosmetic_id));
	}

	let requested = normalize(requested);
	if let Some(requested) = &requested {
		validate_colors(requested, &allowed_slots(&cosmetic.r#type, declared))?;
	}

	let active = active || requested.is_none();
	let is_aura = cosmetic.r#type == CosmeticType::Aura;
	let particle_color = if is_aura && active {
		particle_color_from(requested.as_ref())
	} else {
		None
	};

	Ok(ColorChange {
		is_aura,
		particle_color,
		stored: requested,
		active,
	})
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StoredColors {
	pub palettes: HashMap<i32, SlotColors>,
	pub aura_defaults: HashSet<i32>,
	pub saved: HashMap<i32, SlotColors>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct OwnedPalettes {
	pub colors: HashMap<i32, SlotColors>,
	pub active: Vec<i32>,
}

impl StoredColors {
	pub(crate) fn owned_palettes(self, owned: &HashSet<i32>) -> OwnedPalettes {
		let mut active: Vec<i32> = self
			.palettes
			.keys()
			.copied()
			.filter(|cosmetic_id| owned.contains(cosmetic_id))
			.collect();
		active.sort_unstable();
		let mut colors = self.saved;
		colors.extend(self.palettes);
		colors.retain(|cosmetic_id, _| owned.contains(cosmetic_id));
		OwnedPalettes { colors, active }
	}
}

pub(crate) fn apply_stored(
	colors: &mut HashMap<i32, SlotColors>,
	cosmetic_id: i32,
	stored: Option<&SlotColors>,
) {
	match stored {
		Some(stored) => colors.insert(cosmetic_id, stored.clone()),
		None => colors.remove(&cosmetic_id),
	};
}

fn keeps_aura_default(is_aura: bool, stored: Option<&SlotColors>) -> bool {
	is_aura && stored.is_none_or(|stored| !stored.contains_key(AURA_SLOT))
}

fn apply_choice(
	runtime: &mut PlayerRuntimeState,
	cosmetic_id: i32,
	stored: Option<&SlotColors>,
	is_aura: bool,
) {
	apply_stored(&mut runtime.colors, cosmetic_id, stored);
	if keeps_aura_default(is_aura, stored) {
		runtime.aura_defaults.insert(cosmetic_id);
	} else {
		runtime.aura_defaults.remove(&cosmetic_id);
	}
}

pub(crate) fn equipped_aura(runtime: &PlayerRuntimeState) -> Option<i32> {
	runtime.equipped.get(&BodySlot::Aura).copied()
}

pub(crate) fn is_worn(runtime: &PlayerRuntimeState, cosmetic_id: i32) -> bool {
	runtime.equipped.values().any(|&worn| worn == cosmetic_id)
}

pub(crate) fn particle_alias(
	runtime: &PlayerRuntimeState,
	cosmetic_id: i32,
	change: &ColorChange,
) -> Option<Option<i32>> {
	(change.is_aura && equipped_aura(runtime) == Some(cosmetic_id))
		.then_some(change.particle_color)
}

pub(crate) fn apply_change(
	runtime: &mut PlayerRuntimeState,
	cosmetic_id: i32,
	change: &ColorChange,
) -> Option<Option<i32>> {
	apply_choice(runtime, cosmetic_id, change.worn(), change.is_aura);
	let alias = particle_alias(runtime, cosmetic_id, change)?;
	runtime.particle_color = alias;
	Some(alias)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedPalette {
	pub colors: SlotColors,
	pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParticleChange {
	pub aura: i32,
	pub stored: Option<SlotColors>,
	pub active: bool,
}

impl ParticleChange {
	pub(crate) fn worn(&self) -> Option<&SlotColors> {
		self.stored.as_ref().filter(|_| self.active)
	}
}

pub(crate) fn plan_particle_color(
	aura: i32,
	saved: Option<SavedPalette>,
	color: Option<i32>,
) -> ParticleChange {
	match saved {
		Some(saved) if color.is_none() && !saved.active => ParticleChange {
			aura,
			stored: Some(saved.colors),
			active: false,
		},
		saved => ParticleChange {
			aura,
			stored: merge_aura(saved.map(|saved| saved.colors).as_ref(), color),
			active: true,
		},
	}
}

pub(crate) fn apply_particle_color(
	runtime: &mut PlayerRuntimeState,
	color: Option<i32>,
	planned: Option<&ParticleChange>,
) {
	runtime.particle_color = color;
	if let Some(planned) = planned {
		apply_choice(runtime, planned.aura, planned.worn(), true);
	}
}

pub(crate) fn saved_aura_color(
	palettes: &HashMap<i32, SlotColors>,
	aura_defaults: &HashSet<i32>,
	aura: i32,
) -> Option<Option<i32>> {
	match particle_color_from(palettes.get(&aura)) {
		Some(color) => Some(Some(color)),
		None if aura_defaults.contains(&aura) || palettes.contains_key(&aura) => Some(None),
		None => None,
	}
}

pub(crate) fn adopted_color(
	current: Option<i32>,
	aura: Option<i32>,
	palettes: &HashMap<i32, SlotColors>,
	aura_defaults: &HashSet<i32>,
) -> Option<Option<i32>> {
	let saved = saved_aura_color(palettes, aura_defaults, aura?)?;
	(saved != current).then_some(saved)
}

pub(crate) fn adopt_worn_aura(runtime: &mut PlayerRuntimeState) -> Option<Option<i32>> {
	let adopted = adopted_color(
		runtime.particle_color,
		equipped_aura(runtime),
		&runtime.colors,
		&runtime.aura_defaults,
	)?;
	runtime.particle_color = adopted;
	Some(adopted)
}

pub(crate) async fn owned_cosmetic(
	db: &impl ConnectionTrait,
	player_id: i32,
	cosmetic_id: i32,
) -> Result<Option<cosmetic::Model>, DbErr> {
	Cosmetic::find_by_id(cosmetic_id)
		.join(
			JoinType::InnerJoin,
			cosmetic::Relation::PlayerOwnedCosmetic.def(),
		)
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player_id))
		.one(db)
		.await
}

pub(crate) async fn declared_slots(
	db: &impl ConnectionTrait,
	asset_id: Option<i32>,
) -> Result<Vec<String>, DbErr> {
	let Some(asset_id) = asset_id else {
		return Ok(Vec::new());
	};

	Ok(AssetColorSlot::find()
		.filter(asset_color_slot::Column::AssetId.eq(asset_id))
		.all(db)
		.await?
		.into_iter()
		.map(|row| row.slot)
		.collect())
}

pub(crate) async fn slots_for_asset(
	state: &ApiState,
	asset_id: Option<i32>,
	kind: &CosmeticType,
) -> Result<Vec<String>, DbErr> {
	let declared = declared_slots(&state.database, asset_id).await?;
	match asset_id {
		Some(asset_id) if declared.is_empty() => {
			scan_stored_slots(state, asset_id, kind).await
		}
		_ => Ok(declared),
	}
}

fn group_rows(
	rows: impl IntoIterator<Item = player_cosmetic_colors::Model>,
) -> StoredColors {
	rows.into_iter().fold(StoredColors::default(), |mut acc, row| {
		if row.slot == AURA_SLOT && !(row.active && row.color.is_some()) {
			acc.aura_defaults.insert(row.cosmetic_id);
		}
		if let Some(color) = row.color {
			let palettes = if row.active {
				&mut acc.palettes
			} else {
				&mut acc.saved
			};
			palettes
				.entry(row.cosmetic_id)
				.or_default()
				.insert(row.slot, color);
		}
		acc
	})
}

pub(crate) async fn load_colors(
	db: &impl ConnectionTrait,
	player_id: i32,
) -> Result<StoredColors, DbErr> {
	Ok(group_rows(
		PlayerCosmeticColors::find()
			.filter(player_cosmetic_colors::Column::PlayerId.eq(player_id))
			.all(db)
			.await?,
	))
}

fn palette_from(mut stored: StoredColors, cosmetic_id: i32) -> Option<SavedPalette> {
	match stored.palettes.remove(&cosmetic_id) {
		Some(colors) => Some(SavedPalette {
			colors,
			active: true,
		}),
		None => stored.saved.remove(&cosmetic_id).map(|colors| SavedPalette {
			colors,
			active: false,
		}),
	}
}

pub(crate) async fn load_palette(
	db: &impl ConnectionTrait,
	player_id: i32,
	cosmetic_id: i32,
) -> Result<Option<SavedPalette>, DbErr> {
	let rows = PlayerCosmeticColors::find()
		.filter(player_cosmetic_colors::Column::PlayerId.eq(player_id))
		.filter(player_cosmetic_colors::Column::CosmeticId.eq(cosmetic_id))
		.all(db)
		.await?;
	Ok(palette_from(group_rows(rows), cosmetic_id))
}

pub(crate) async fn load_owned_palettes(
	db: &impl ConnectionTrait,
	player_id: i32,
) -> Result<OwnedPalettes, DbErr> {
	let owned = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player_id))
		.all(db)
		.await?
		.into_iter()
		.map(|row| row.cosmetic_id)
		.collect::<HashSet<_>>();
	if owned.is_empty() {
		return Ok(OwnedPalettes::default());
	}
	Ok(load_colors(db, player_id).await?.owned_palettes(&owned))
}

pub(crate) async fn load_colors_for_players(
	db: &impl ConnectionTrait,
	players: &[Uuid],
) -> Result<HashMap<Uuid, StoredColors>, DbErr> {
	if players.is_empty() {
		return Ok(HashMap::new());
	}

	let mut rows: HashMap<Uuid, Vec<player_cosmetic_colors::Model>> = HashMap::new();
	for (row, user) in PlayerCosmeticColors::find()
		.find_also_related(User)
		.filter(user::Column::MinecraftUuid.is_in(players.to_vec()))
		.all(db)
		.await?
	{
		if let Some(user) = user {
			rows.entry(user.minecraft_uuid).or_default().push(row);
		}
	}

	Ok(rows
		.into_iter()
		.map(|(player, rows)| (player, group_rows(rows)))
		.collect())
}

fn stored_rows(colors: Option<&SlotColors>, is_aura: bool) -> Vec<(String, Option<i32>)> {
	let mut rows: Vec<(String, Option<i32>)> = colors
		.into_iter()
		.flatten()
		.map(|(slot, &color)| (slot.clone(), Some(color)))
		.collect();
	if keeps_aura_default(is_aura, colors) {
		rows.push((AURA_SLOT.to_owned(), None));
	}
	rows
}

pub(crate) async fn store_colors(
	db: &(impl ConnectionTrait + TransactionTrait),
	player_id: i32,
	cosmetic_id: i32,
	colors: Option<&SlotColors>,
	is_aura: bool,
	active: bool,
) -> Result<(), DbErr> {
	let txn = db.begin().await?;

	PlayerCosmeticColors::delete_many()
		.filter(player_cosmetic_colors::Column::PlayerId.eq(player_id))
		.filter(player_cosmetic_colors::Column::CosmeticId.eq(cosmetic_id))
		.exec(&txn)
		.await?;

	let rows = stored_rows(colors.filter(|colors| !colors.is_empty()), is_aura);
	if !rows.is_empty() {
		let now = Utc::now();
		PlayerCosmeticColors::insert_many(rows.into_iter().map(|(slot, color)| {
			player_cosmetic_colors::ActiveModel {
				player_id: Set(player_id),
				cosmetic_id: Set(cosmetic_id),
				slot: Set(slot),
				color: Set(color),
				updated_at: Set(now.into()),
				active: Set(active),
			}
		}))
		.exec(&txn)
		.await?;
	}

	txn.commit().await
}

#[cfg(test)]
mod tests {
	use std::collections::{BTreeSet, HashMap, HashSet};

	use chrono::Utc;
	use entities::{
		player_cosmetic_colors,
		sea_orm_active_enums::{BodySlot, CosmeticType},
	};

	use super::{
		AURA_SLOT, ColorChange, MAX_COLOR_SLOTS, OwnedPalettes, ParticleChange, SavedPalette,
		SlotColors, StoredColors, adopt_worn_aura, adopted_color, allowed_slots, apply_change,
		apply_particle_color, apply_stored, aura_colors, equipped_aura, group_rows, is_worn,
		merge_aura, normalize, palette_from, particle_alias, particle_color_from, plan_change,
		plan_particle_color, snapshot_colors, stored_rows, validate_colors,
	};
	use crate::api::{
		state::PlayerRuntimeState,
		v0::{cosmetics::tests::cosmetic, websocket::structs::WebsocketError},
	};

	fn c(pairs: &[(&str, i32)]) -> SlotColors {
		pairs.iter().map(|(slot, color)| ((*slot).to_owned(), *color)).collect()
	}

	fn slots(names: &[&str]) -> BTreeSet<String> {
		names.iter().map(|name| (*name).to_owned()).collect()
	}

	fn argb(rgb: i32) -> i32 {
		(rgb as u32 | 0xFF00_0000) as i32
	}

	fn plan(id: i32, kind: CosmeticType, palette: Option<SlotColors>, active: bool) -> ColorChange {
		let declared = ["fur".to_owned(), "eyes".to_owned(), "glow".to_owned()];
		plan_change(id, Some(&cosmetic(id, kind)), |_| true, palette, active, declared).unwrap()
	}

	fn hat(palette: Option<SlotColors>, active: bool) -> ColorChange {
		plan(4, CosmeticType::Hat, palette, active)
	}

	fn aura(id: i32, rgb: Option<i32>) -> ColorChange {
		plan(id, CosmeticType::Aura, rgb.map(|rgb| c(&[(AURA_SLOT, rgb)])), true)
	}

	fn wearing_aura(aura: i32, particle_color: Option<i32>) -> PlayerRuntimeState {
		PlayerRuntimeState {
			equipped: HashMap::from([(BodySlot::Aura, aura)]),
			particle_color,
			..Default::default()
		}
	}

	fn row(cosmetic_id: i32, slot: &str, color: Option<i32>, active: bool) -> player_cosmetic_colors::Model {
		let (player_id, slot, updated_at) = (1, slot.to_owned(), Utc::now().into());
		player_cosmetic_colors::Model { player_id, cosmetic_id, slot, color, updated_at, active }
	}

	fn saved(pairs: &[(&str, i32)], active: bool) -> Option<SavedPalette> {
		Some(SavedPalette { colors: c(pairs), active })
	}

	#[test]
	fn palettes_are_validated_against_the_declared_slots() {
		let check = |palette: &SlotColors, allowed: &[&str]| match validate_colors(palette, &slots(allowed)) {
			Ok(()) => None,
			Err(WebsocketError::InvalidColors(reason)) => Some(reason),
			Err(other) => panic!("expected InvalidColors, got {other:?}"),
		};
		assert_eq!(check(&c(&[("fur", 0), ("eyes", 0xFF_FFFF)]), &["fur", "eyes"]), None);
		assert_eq!(check(&c(&[("ribbon", 1)]), &["fur"]), Some("slot is not customisable"));
		assert_eq!(check(&c(&[("fur", 1), ("ribbon", 1)]), &["fur"]), Some("too many colour slots"));
		for bad in [-1, 0x0100_0000, argb(0)] {
			assert_eq!(check(&c(&[("fur", bad)]), &["fur"]), Some("colours must be RGB"));
		}
		let too_long = "a".repeat(65);
		for bad in ["", "Fur", "fur-coat", "fur coat", too_long.as_str()] {
			assert_eq!(check(&c(&[(bad, 0)]), &[bad]), Some("invalid slot id"), "{bad:?}");
		}
		let longest = "a".repeat(64);
		assert_eq!(check(&c(&[(&longest, 0)]), &[&longest]), None);

		let names = (0..=MAX_COLOR_SLOTS).map(|i| format!("slot_{i}")).collect::<Vec<_>>();
		let allowed = names.iter().map(String::as_str).collect::<Vec<_>>();
		let palette = |count| names.iter().take(count).map(|name| (name.clone(), 0)).collect();
		assert_eq!(check(&palette(MAX_COLOR_SLOTS + 1), &allowed), Some("too many colour slots"));
		assert_eq!(check(&palette(MAX_COLOR_SLOTS), &allowed), None);

		assert_eq!(normalize(Some(SlotColors::new())), None);
		assert_eq!(normalize(None), None);
		assert_eq!(normalize(Some(c(&[("fur", 1)]))), Some(c(&[("fur", 1)])));
		assert_eq!(allowed_slots(&CosmeticType::Aura, []), slots(&[AURA_SLOT]));
		assert_eq!(allowed_slots(&CosmeticType::Aura, ["glow".to_owned()]), slots(&[AURA_SLOT, "glow"]));
		assert!(allowed_slots(&CosmeticType::Hat, []).is_empty());
		assert_eq!(allowed_slots(&CosmeticType::Hat, ["fur".to_owned()]), slots(&["fur"]));
	}

	#[test]
	fn the_particle_colour_is_the_aura_slot() {
		for color in [argb(0xFF_4444), argb(0x3A_3F43), -1, argb(0)] {
			let palette = aura_colors(Some(color)).expect("a colour gives an aura palette");
			assert_eq!(palette, c(&[(AURA_SLOT, color & 0x00FF_FFFF)]));
			assert!(validate_colors(&palette, &allowed_slots(&CosmeticType::Aura, [])).is_ok());
			assert_eq!(particle_color_from(Some(&palette)), Some(color));
		}
		assert_eq!((aura_colors(None), particle_color_from(None)), (None, None));

		let saved = c(&[(AURA_SLOT, 0x12_34_56), ("glow", 0x00_FF00)]);
		assert_eq!(merge_aura(Some(&saved), Some(argb(0xFF))), Some(c(&[(AURA_SLOT, 0xFF), ("glow", 0x00_FF00)])));
		assert_eq!(merge_aura(Some(&saved), None), Some(c(&[("glow", 0x00_FF00)])));
		assert_eq!(merge_aura(None, None), None);

		let stored = HashMap::from([(1, c(&[("fur", 1)])), (2, c(&[("eyes", 2)])), (3, c(&[("glow", 3)]))]);
		let equipped = HashMap::from([(BodySlot::Hat, 1), (BodySlot::Aura, 3)]);
		let snapshot = snapshot_colors(&equipped, &stored, Some(argb(0x00_FF00)));
		let expected = HashMap::from([(1, c(&[("fur", 1)])), (3, c(&[(AURA_SLOT, 0x00_FF00), ("glow", 3)]))]);
		assert_eq!(snapshot, expected);
		let plain_aura = HashMap::from([(BodySlot::Aura, 9)]);
		assert_eq!(snapshot_colors(&plain_aura, &stored, Some(-1))[&9], c(&[(AURA_SLOT, 0xFF_FFFF)]));
		assert!(snapshot_colors(&plain_aura, &stored, None).is_empty());
		assert!(snapshot_colors(&HashMap::new(), &stored, Some(-1)).is_empty());
	}

	#[test]
	fn a_colour_change_is_planned_only_for_owned_entitled_valid_palettes() {
		let hat_model = cosmetic(4, CosmeticType::Hat);
		let fur = || Some(c(&[("fur", 1)]));
		let declared = || vec!["fur".to_owned(), "eyes".to_owned()];
		assert!(matches!(plan_change(9, None, |_| true, fur(), true, declared()), Err(WebsocketError::UnownedCosmetic(9))));
		assert!(matches!(plan_change(9, None, |_| true, None, true, Vec::new()), Err(WebsocketError::UnownedCosmetic(9))));
		assert!(matches!(
			plan_change(4, Some(&hat_model), |_| false, fur(), true, declared()),
			Err(WebsocketError::CustomColorsNotAllowed(4))
		));
		let ribbon = Some(c(&[("ribbon", 1)]));
		assert!(matches!(plan_change(4, Some(&hat_model), |_| true, ribbon, true, declared()), Err(WebsocketError::InvalidColors(_))));
		assert!(matches!(plan_change(4, Some(&hat_model), |_| true, fur(), true, []), Err(WebsocketError::InvalidColors(_))));

		let set = hat(fur(), true);
		let expected = ColorChange { is_aura: false, particle_color: None, stored: fur(), active: true };
		assert_eq!(set, expected);
		assert_eq!(set.announced(), fur());
		let inactive = hat(fur(), false);
		assert_eq!((inactive.stored.clone(), inactive.active), (fur(), false));
		assert_eq!((inactive.worn(), inactive.announced()), (None, None));
		assert_eq!(stored_rows(inactive.stored.as_ref(), false), vec![("fur".to_owned(), Some(1))]);
		for (palette, active) in [(None, true), (None, false), (Some(SlotColors::new()), true)] {
			let cleared = hat(palette, active);
			assert_eq!((cleared.stored.as_ref(), cleared.active, cleared.announced()), (None, true, None));
			assert!(stored_rows(cleared.stored.as_ref(), cleared.is_aura).is_empty());
		}

		let worn = aura(3, Some(0xFF_4444));
		let expected = ColorChange {
			is_aura: true,
			particle_color: Some(argb(0xFF_4444)),
			stored: Some(c(&[(AURA_SLOT, 0xFF_4444)])),
			active: true,
		};
		assert_eq!(worn, expected);
		assert_eq!(worn.announced(), Some(c(&[(AURA_SLOT, 0xFF_4444)])));
		let cleared = aura(3, None);
		assert_eq!((cleared.particle_color, cleared.announced()), (None, None));

		let mut stored = HashMap::from([(1, c(&[("fur", 1)]))]);
		apply_stored(&mut stored, 4, set.stored.as_ref());
		apply_stored(&mut stored, 4, hat(Some(c(&[("eyes", 2)])), true).stored.as_ref());
		apply_stored(&mut stored, 3, None);
		assert_eq!(stored, HashMap::from([(1, c(&[("fur", 1)])), (4, c(&[("eyes", 2)]))]));
		apply_stored(&mut stored, 4, None);
		assert!(!stored.contains_key(&4));
	}

	#[test]
	fn worn_palettes_reach_the_runtime_and_unworn_ones_stay_saved() {
		let mut runtime = PlayerRuntimeState {
			equipped: HashMap::from([(BodySlot::Hat, 4), (BodySlot::Aura, 5), (BodySlot::Cape, 7)]),
			..Default::default()
		};
		assert!(is_worn(&runtime, 4) && is_worn(&runtime, 7) && !is_worn(&runtime, 6));
		assert!(!is_worn(&PlayerRuntimeState::default(), 5));

		let fur = || Some(c(&[("fur", 1)]));
		apply_change(&mut runtime, 4, &hat(fur(), true));
		assert_eq!(apply_change(&mut runtime, 4, &hat(fur(), false)), None);
		assert!(!runtime.colors.contains_key(&4));
		assert!(snapshot_colors(&runtime.equipped, &runtime.colors, None).is_empty());
		apply_change(&mut runtime, 4, &hat(fur(), true));
		assert_eq!(snapshot_colors(&runtime.equipped, &runtime.colors, None)[&4], c(&[("fur", 1)]));

		runtime.equipped.remove(&BodySlot::Hat);
		for active in [true, false] {
			assert_eq!(apply_change(&mut runtime, 4, &hat(Some(c(&[("fur", 7)])), active)), None);
		}
		assert!(!is_worn(&runtime, 4));

		let palette = c(&[(AURA_SLOT, 0x22_2222), ("glow", 3)]);
		let glow = |active| plan(5, CosmeticType::Aura, Some(palette.clone()), active);
		assert_eq!(apply_change(&mut runtime, 5, &glow(true)), Some(Some(argb(0x22_2222))));
		let inactive = glow(false);
		assert_eq!(inactive.particle_color, None);
		assert_eq!(apply_change(&mut runtime, 5, &inactive), Some(None));
		assert_eq!(runtime.particle_color, None);
		assert!(!runtime.colors.contains_key(&5) && runtime.aura_defaults.contains(&5));
		assert_eq!(adopt_worn_aura(&mut runtime), None);
		assert_eq!(apply_change(&mut runtime, 5, &glow(true)), Some(Some(argb(0x22_2222))));
		assert_eq!(runtime.colors[&5], palette);
		assert!(!runtime.aura_defaults.contains(&5));
	}

	#[test]
	fn each_aura_keeps_its_own_colour_and_only_the_worn_one_drives_the_particles() {
		let worn = argb(0x11_1111);
		let mut runtime = wearing_aura(3, Some(worn));
		assert_eq!(apply_change(&mut runtime, 5, &aura(5, Some(0x22_2222))), None);
		assert_eq!(runtime.particle_color, Some(worn));
		assert_eq!(runtime.colors[&5], c(&[(AURA_SLOT, 0x22_2222)]));
		let snapshot = snapshot_colors(&runtime.equipped, &runtime.colors, runtime.particle_color);
		assert_eq!(snapshot[&3], c(&[(AURA_SLOT, 0x11_1111)]));

		apply_change(&mut runtime, 3, &aura(3, Some(0x11_1111)));
		runtime.equipped.insert(BodySlot::Aura, 5);
		assert_eq!(adopt_worn_aura(&mut runtime), Some(Some(argb(0x22_2222))));
		runtime.equipped.insert(BodySlot::Aura, 3);
		assert_eq!(adopt_worn_aura(&mut runtime), Some(Some(worn)));
		assert_eq!(adopt_worn_aura(&mut runtime), None);
		runtime.equipped.insert(BodySlot::Aura, 9);
		assert_eq!(adopt_worn_aura(&mut runtime), None);
		assert_eq!(runtime.particle_color, Some(worn));
		runtime.equipped.remove(&BodySlot::Aura);
		assert_eq!(adopt_worn_aura(&mut runtime), None);

		runtime.equipped.insert(BodySlot::Aura, 3);
		assert_eq!(apply_change(&mut runtime, 5, &aura(5, None)), None);
		assert_eq!(runtime.particle_color, Some(worn));
		runtime.equipped.insert(BodySlot::Aura, 5);
		assert_eq!(adopt_worn_aura(&mut runtime), Some(None));
		assert_eq!(runtime.particle_color, None);

		let mut runtime = wearing_aura(5, Some(-1));
		let change = aura(5, Some(0x22_2222));
		assert_eq!(particle_alias(&runtime, 5, &change), Some(Some(argb(0x22_2222))));
		assert_eq!(particle_alias(&runtime, 3, &change), None);
		assert_eq!(runtime.particle_color, Some(-1));
		assert_eq!(apply_change(&mut runtime, 5, &change), Some(Some(argb(0x22_2222))));
		assert_eq!(runtime.particle_color, Some(argb(0x22_2222)));
		assert_eq!(apply_change(&mut runtime, 5, &aura(5, None)), Some(None));
		assert_eq!(runtime.particle_color, None);
		assert!(!runtime.colors.contains_key(&5));

		let palettes = HashMap::from([(5, c(&[("glow", 0x00_FF00)]))]);
		let none = HashSet::new();
		assert_eq!(adopted_color(Some(-1), Some(5), &palettes, &none), Some(None));
		assert_eq!(adopted_color(None, Some(5), &palettes, &none), None);
		assert_eq!(adopted_color(Some(-1), Some(9), &palettes, &none), None);
		assert_eq!(adopted_color(Some(-1), Some(9), &palettes, &HashSet::from([9])), Some(None));
		assert_eq!(adopted_color(Some(-1), None, &palettes, &none), None);
	}

	#[test]
	fn stored_rows_round_trip_palettes_saved_ones_and_design_colours() {
		assert_eq!(stored_rows(None, true), vec![(AURA_SLOT.to_owned(), None)]);
		assert_eq!(
			stored_rows(Some(&c(&[("glow", 1)])), true),
			vec![("glow".to_owned(), Some(1)), (AURA_SLOT.to_owned(), None)]
		);
		assert_eq!(stored_rows(Some(&c(&[(AURA_SLOT, 2)])), true), vec![(AURA_SLOT.to_owned(), Some(2))]);
		assert!(stored_rows(None, false).is_empty());
		assert_eq!(stored_rows(Some(&c(&[("fur", 3)])), false), vec![("fur".to_owned(), Some(3))]);

		let stored = group_rows([
			row(4, "fur", Some(1), true),
			row(8, AURA_SLOT, None, true),
			row(7, "fur", Some(2), false),
			row(5, AURA_SLOT, Some(3), false),
			row(5, "glow", Some(4), false),
			row(6, "glow", Some(5), false),
			row(6, AURA_SLOT, None, false),
		]);
		assert_eq!(stored.palettes, HashMap::from([(4, c(&[("fur", 1)]))]));
		let saved_palettes = [(7, c(&[("fur", 2)])), (5, c(&[(AURA_SLOT, 3), ("glow", 4)])), (6, c(&[("glow", 5)]))];
		assert_eq!(stored.saved, HashMap::from(saved_palettes));
		assert_eq!(stored.aura_defaults, HashSet::from([5, 6, 8]));
		assert_eq!(adopted_color(Some(-1), Some(5), &stored.palettes, &stored.aura_defaults), Some(None));

		assert_eq!(palette_from(stored.clone(), 4), saved(&[("fur", 1)], true));
		assert_eq!(palette_from(stored.clone(), 7), saved(&[("fur", 2)], false));
		assert_eq!(palette_from(stored.clone(), 8), None);
		let owned = stored.owned_palettes(&HashSet::from([4, 5, 8]));
		let colors = HashMap::from([(4, c(&[("fur", 1)])), (5, c(&[(AURA_SLOT, 3), ("glow", 4)]))]);
		assert_eq!(owned, OwnedPalettes { colors, active: vec![4] });

		let stored = StoredColors {
			palettes: HashMap::from([(1, c(&[("fur", 1)])), (2, c(&[("fur", 2)]))]),
			aura_defaults: HashSet::new(),
			saved: HashMap::from([(3, c(&[("eyes", 3)]))]),
		};
		let equipped = HashMap::from([(BodySlot::Hat, 1)]);
		assert_eq!(snapshot_colors(&equipped, &stored.palettes, None).len(), 1);
		assert_eq!(stored.owned_palettes(&HashSet::from([1, 2, 3])).active, vec![1, 2]);
	}

	fn set_particle_color(
		runtime: &mut PlayerRuntimeState,
		saved: Option<SavedPalette>,
		color: Option<i32>,
	) -> (ParticleChange, Option<SlotColors>) {
		let aura = equipped_aura(runtime).expect("an aura should be worn");
		let planned = plan_particle_color(aura, saved, color);
		apply_particle_color(runtime, color, Some(&planned));
		let announced = merge_aura(runtime.colors.get(&aura), color);
		(planned, announced)
	}

	fn reload(planned: &ParticleChange) -> StoredColors {
		let rows = stored_rows(planned.stored.as_ref(), true).into_iter();
		group_rows(rows.map(|(slot, color)| row(planned.aura, &slot, color, planned.active)))
	}

	#[test]
	fn the_legacy_particle_colour_lands_on_the_worn_aura_palette() {
		let glowing = [(AURA_SLOT, 1), ("glow", 0x00_FF00)];
		let mut runtime = wearing_aura(5, None);
		runtime.colors.extend([(5, c(&glowing)), (3, c(&[(AURA_SLOT, 2)]))]);
		let (planned, announced) = set_particle_color(&mut runtime, saved(&glowing, true), Some(argb(0x33_3333)));
		let palette = c(&[(AURA_SLOT, 0x33_3333), ("glow", 0x00_FF00)]);
		assert_eq!(planned, ParticleChange { aura: 5, stored: Some(palette.clone()), active: true });
		assert_eq!(announced, Some(palette.clone()));
		assert_eq!((&runtime.colors[&5], &runtime.colors[&3]), (&palette, &c(&[(AURA_SLOT, 2)])));

		runtime.equipped.remove(&BodySlot::Aura);
		runtime.equipped.insert(BodySlot::Aura, 5);
		assert_eq!(adopt_worn_aura(&mut runtime), None);
		assert_eq!(runtime.particle_color, Some(argb(0x33_3333)));
		let reloaded = reload(&planned);
		let mut fresh = wearing_aura(5, Some(argb(0x11_1111)));
		(fresh.colors, fresh.aura_defaults) = (reloaded.palettes, reloaded.aura_defaults);
		assert_eq!(adopt_worn_aura(&mut fresh), Some(Some(argb(0x33_3333))));

		let (cleared, announced) = set_particle_color(&mut runtime, saved(&[(AURA_SLOT, 0x33_3333), ("glow", 0x00_FF00)], true), None);
		assert!(cleared.active);
		assert_eq!(cleared.stored, Some(c(&[("glow", 0x00_FF00)])));
		assert_eq!(announced, Some(c(&[("glow", 0x00_FF00)])));
		assert!(runtime.aura_defaults.contains(&5));
		assert_eq!(runtime.colors[&5], c(&[("glow", 0x00_FF00)]));

		let mut bare = PlayerRuntimeState::default();
		assert_eq!(equipped_aura(&bare), None);
		apply_particle_color(&mut bare, Some(-1), None);
		assert_eq!(bare.particle_color, Some(-1));
	}

	#[test]
	fn the_legacy_particle_colour_merges_into_or_keeps_the_saved_palette() {
		let mut runtime = wearing_aura(5, None);
		runtime.aura_defaults.insert(5);
		let (planned, announced) = set_particle_color(&mut runtime, saved(&[(AURA_SLOT, 1), ("glow", 2)], false), None);
		let palette = c(&[(AURA_SLOT, 1), ("glow", 2)]);
		assert_eq!(planned, ParticleChange { aura: 5, stored: Some(palette.clone()), active: false });
		assert_eq!((planned.worn(), announced), (None, None));
		assert!(!runtime.colors.contains_key(&5) && runtime.aura_defaults.contains(&5));
		assert_eq!(runtime.particle_color, None);
		assert_eq!(palette_from(reload(&planned), 5), Some(SavedPalette { colors: palette, active: false }));

		let color = Some(argb(0x33_3333));
		let (planned, announced) = set_particle_color(&mut runtime, saved(&[(AURA_SLOT, 1), ("glow", 2), ("ring", 3)], false), color);
		let palette = c(&[(AURA_SLOT, 0x33_3333), ("glow", 2), ("ring", 3)]);
		assert_eq!((planned.stored.as_ref(), planned.active), (Some(&palette), true));
		assert_eq!(announced, Some(palette.clone()));
		assert_eq!(runtime.colors[&5], palette);
		assert!(!runtime.aura_defaults.contains(&5));
		assert_eq!(runtime.particle_color, color);

		let mut runtime = wearing_aura(5, None);
		let (planned, announced) = set_particle_color(&mut runtime, None, Some(-1));
		assert_eq!((planned.stored, planned.active), (Some(c(&[(AURA_SLOT, 0xFF_FFFF)])), true));
		assert_eq!(announced, Some(c(&[(AURA_SLOT, 0xFF_FFFF)])));
		let (cleared, announced) = set_particle_color(&mut runtime, saved(&[(AURA_SLOT, 0xFF_FFFF)], true), None);
		assert_eq!((cleared.stored.as_ref(), cleared.active, announced), (None, true, None));
		assert_eq!(stored_rows(cleared.stored.as_ref(), true), vec![(AURA_SLOT.to_owned(), None)]);
		assert!(!runtime.colors.contains_key(&5) && runtime.aura_defaults.contains(&5));
	}
}
