use std::{
	collections::{HashMap, HashSet, hash_map::Entry},
	mem,
};

use aide::{
	axum::{ApiRouter, routing::ApiMethodDocs},
	openapi::Operation,
	transform::TransformOperation,
};
use axum::{
	body::Body,
	extract::{
		Query, State, WebSocketUpgrade,
		ws::{Message, WebSocket},
	},
	response::IntoResponse,
	routing::get,
};
use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::{BodySlot, PlayerStatus};
use http::{Response, StatusCode};
use sea_orm::{
	ColumnTrait as _, DatabaseTransaction, EntityTrait as _, Iterable as _, QueryFilter,
};
use tokio::sync::{OwnedMutexGuard, mpsc};
use tracing::warn;
use uuid::Uuid;

use crate::api::{
	ApiState,
	state::{
		ActiveEmote, ConnectionId, EquipmentPersistence, ParticleColorPersistence,
		PlayerRuntimeState, PlaytimeSession, RealtimeConnection, RealtimeState,
		Supersession, effective_status,
	},
	v0::{
		account::{AuthenticatedPlayer, ClientKind, TokenAccess},
		bonuses::{self, Discovery, refresh_bonuses, rescan_bonuses},
		cosmetics::EmoteInfo,
		outfits::mirror_active_outfit_particle_color,
		social,
		websocket::{
			colors::{self, ColorChange, ParticleChange, SlotColors, StoredColors},
			pet_preferences::{self, Friendships},
			shared_emote::{
				self, DatabaseGate, announce_stop, cancel_requests_of, release,
			},
			structs::{
				ClientBoundPacket, EmoteAnchor, PetPlay, ServerBoundPacket, SharedEmote,
				WebsocketError,
			},
		},
	},
};

/// Max UUIDs in a single `SubscribePlayers` or `GetActiveCosmetics` message.
const MAX_PLAYERS_PER_REQUEST: usize = 64;
const MAX_PLAYER_SUBSCRIPTIONS: usize = 512;
const REAL_PLAYER_UUID_VERSION: usize = 4;

fn enforce_max_players_per_request(players: &[Uuid]) -> Result<(), WebsocketError> {
	if players.len() > MAX_PLAYERS_PER_REQUEST {
		return Err(WebsocketError::TooManyPlayersInRequest {
			limit: MAX_PLAYERS_PER_REQUEST,
		});
	}
	Ok(())
}

pub(super) fn is_fake_player(player: &Uuid) -> bool {
	player.get_version_num() != REAL_PLAYER_UUID_VERSION
}

/// Reclaims capacity once a map is mostly holes.
fn shrink_map_if_sparse<K, V>(map: &mut HashMap<K, V>)
where
	K: Eq + std::hash::Hash,
{
	if map.capacity() > map.len().saturating_mul(4) {
		map.shrink_to_fit();
	}
}

fn shrink_set_if_sparse<T>(set: &mut HashSet<T>)
where
	T: Eq + std::hash::Hash,
{
	if set.capacity() > set.len().saturating_mul(4) {
		set.shrink_to_fit();
	}
}

/// Removes `connection_id` from each given player's watcher set, returning the
/// players that are left with no watchers at all.
fn drop_watchers(
	watchers: &mut HashMap<Uuid, HashSet<ConnectionId>>,
	connection_id: ConnectionId,
	players: impl IntoIterator<Item = Uuid>,
) -> Vec<Uuid> {
	let mut orphaned = Vec::new();

	for player in players {
		if let Some(player_watchers) = watchers.get_mut(&player) {
			player_watchers.remove(&connection_id);
			if player_watchers.is_empty() {
				watchers.remove(&player);
				orphaned.push(player);
			}
		}
	}

	shrink_map_if_sparse(watchers);
	orphaned
}

/// Drops cached runtime state for players that nobody watches any more and who
/// hold no connection of their own.
async fn prune_player_runtime(realtime: &RealtimeState, candidates: Vec<Uuid>) {
	if candidates.is_empty() {
		return;
	}

	let watchers = realtime.watchers.read().await;
	let connections_by_owner = realtime.connections_by_owner.read().await;
	let mut player_runtime = realtime.player_runtime.write().await;
	let mut pruned = false;
	for player in candidates {
		if !watchers.contains_key(&player) && !connections_by_owner.contains_key(&player) {
			pruned |= player_runtime.remove(&player).is_some();
		}
	}
	if pruned {
		shrink_map_if_sparse(&mut player_runtime);
	}
}

fn empty_snapshot(
	rejected: Vec<Uuid>,
	request_id: Option<u64>,
	received_at: i64,
) -> ClientBoundPacket {
	ClientBoundPacket::SubscriptionSnapshot {
		equipped: HashMap::new(),
		active_emotes: HashMap::new(),
		active_emote_starts: HashMap::new(),
		active_emote_anchors: HashMap::new(),
		shared_emotes: HashMap::new(),
		emote_definitions: HashMap::new(),
		active_emote_definitions: HashMap::new(),
		particle_colors: HashMap::new(),
		colors: HashMap::new(),
		saved_colors: None,
		saved_colors_active: None,
		bonuses: HashMap::new(),
		pet_interactions_off: Vec::new(),
		pet_interactions_friends: HashMap::new(),
		users: Vec::new(),
		rejected,
		request_id,
		server_received_time: received_at,
		server_time: Utc::now().timestamp_millis(),
	}
}

#[derive(Debug, Default)]
pub(super) struct RuntimeSnapshot {
	equipped: HashMap<Uuid, HashMap<BodySlot, i32>>,
	pub(super) active_emotes: HashMap<Uuid, i32>,
	pub(super) active_emote_revisions: HashMap<Uuid, (u64, EmoteInfo)>,
	pub(super) active_emote_starts: HashMap<Uuid, i64>,
	pub(super) active_emote_anchors: HashMap<Uuid, EmoteAnchor>,
	pub(super) shared_emotes: HashMap<Uuid, SharedEmote>,
	particle_colors: HashMap<Uuid, i32>,
	colors: HashMap<Uuid, HashMap<i32, SlotColors>>,
	pet_interactions_off: Vec<Uuid>,
	missing: Vec<Uuid>,
}

pub(super) fn read_runtime(
	player_runtime: &HashMap<Uuid, PlayerRuntimeState>,
	players: &[Uuid],
) -> RuntimeSnapshot {
	let mut snapshot = RuntimeSnapshot::default();
	for player in players {
		let Some(runtime) = player_runtime.get(player) else {
			snapshot.missing.push(*player);
			continue;
		};
		snapshot.equipped.insert(*player, runtime.equipped.clone());
		if let Some(emote) = runtime.active_emote {
			snapshot.active_emotes.insert(*player, emote.emote_id);
			if let Some(definition) = &runtime.active_emote_definition
				&& definition.id == emote.emote_id
			{
				snapshot
					.active_emote_revisions
					.insert(*player, (emote.generation, definition.clone()));
			}
			snapshot
				.active_emote_starts
				.insert(*player, emote.started_at);
			if let Some(anchor) = emote.anchor {
				snapshot.active_emote_anchors.insert(*player, anchor);
			}
			if let Some(shared) = emote.shared {
				snapshot.shared_emotes.insert(*player, shared);
			}
		}
		if let Some(color) = runtime.particle_color {
			snapshot.particle_colors.insert(*player, color);
		}
		let worn = colors::snapshot_colors(
			&runtime.equipped,
			&runtime.colors,
			runtime.particle_color,
		);
		if !worn.is_empty() {
			snapshot.colors.insert(*player, worn);
		}
		if runtime.pet_play == PetPlay::Off {
			snapshot.pet_interactions_off.push(*player);
		}
	}
	snapshot
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.route("/websocket", get(self::endpoint))
		.api_route_docs(
			"/websocket",
			ApiMethodDocs::new("get", {
				let mut operation = Operation::default();

				_ = TransformOperation::new(&mut operation)
					.id("websocket")
					.summary("Open a websocket connection to the server")
					.description(
						"Establishes a websocket connection to the server. Websocket \
						 packets can examined from the ClientBoundPacket and \
						 ServerBoundPacket OpenAPI schemas. This largely follows a \
						 request-response model, but that may not always be true. \
						 Pass `?act_as=<uuid>` to receive the packets of a player who \
						 delegated their chats to you, without appearing as them. Needs a \
						 dashboard session holding `ACT_AS_DELEGATE`.",
					)
					.tag("misc")
					.response_with::<{ StatusCode::SWITCHING_PROTOCOLS.as_u16() }, (), _>(
						|res| {
							res.description(
								"Communication will continue over the WebSocket protocol",
							)
						},
					);

				operation
			}),
		)
}

fn stamp_send_time(packet: &mut ClientBoundPacket) {
	if let ClientBoundPacket::SubscriptionSnapshot { server_time, .. } = packet {
		*server_time = Utc::now().timestamp_millis();
	}
}

async fn send_packet(
	socket: &mut WebSocket,
	mut packet: ClientBoundPacket,
) -> Result<(), WebsocketError> {
	stamp_send_time(&mut packet);
	let serialized = serde_json::to_string(&packet)?;
	socket.send(Message::Text(serialized.into())).await?;
	Ok(())
}

pub(super) async fn write_confirmed<F, E>(
	mut packet: ClientBoundPacket,
	write: impl FnOnce(ClientBoundPacket) -> F,
) -> Result<(), E>
where
	F: Future<Output = Result<(), E>>,
{
	let written = packet.take_write_receipt();
	write(packet).await?;
	written.confirm();
	Ok(())
}

async fn active_cosmetics(
	state: &ApiState,
	players: Vec<Uuid>,
) -> Result<HashMap<Uuid, Vec<i32>>, WebsocketError> {
	use entities::{prelude::*, user};

	Ok(PlayerEquippedCosmetic::find()
		.find_also_related(User)
		.filter(user::Column::MinecraftUuid.is_in(players))
		.all(&state.database)
		.await?
		.into_iter()
		.fold(HashMap::new(), |mut acc, (equipment, user)| {
			if let Some(user) = user {
				acc.entry(user.minecraft_uuid)
					.or_insert_with(Vec::new)
					.push(equipment.cosmetic_id);
			}
			acc
		}))
}

async fn load_equipped(
	state: &ApiState,
	player_id: i32,
) -> Result<HashMap<BodySlot, i32>, WebsocketError> {
	use entities::{player_equipped_cosmetic, prelude::*};

	Ok(PlayerEquippedCosmetic::find()
		.filter(player_equipped_cosmetic::Column::PlayerId.eq(player_id))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|equipment| (equipment.slot, equipment.cosmetic_id))
		.collect())
}

async fn load_equipped_for_players(
	state: &ApiState,
	players: &[Uuid],
) -> Result<HashMap<Uuid, HashMap<BodySlot, i32>>, WebsocketError> {
	use entities::{prelude::*, user};

	if players.is_empty() {
		return Ok(HashMap::new());
	}

	let mut equipped = players
		.iter()
		.copied()
		.map(|player| (player, HashMap::new()))
		.collect::<HashMap<_, _>>();

	for (equipment, user) in PlayerEquippedCosmetic::find()
		.find_also_related(User)
		.filter(user::Column::MinecraftUuid.is_in(players.to_vec()))
		.all(&state.database)
		.await?
	{
		if let Some(user) = user {
			equipped
				.entry(user.minecraft_uuid)
				.or_default()
				.insert(equipment.slot, equipment.cosmetic_id);
		}
	}

	Ok(equipped)
}

async fn load_particle_colors_for_players(
	state: &ApiState,
	players: &[Uuid],
) -> Result<HashMap<Uuid, Option<i32>>, WebsocketError> {
	use entities::{prelude::*, user};

	if players.is_empty() {
		return Ok(HashMap::new());
	}

	Ok(User::find()
		.filter(user::Column::MinecraftUuid.is_in(players.to_vec()))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.minecraft_uuid, user.particle_color))
		.collect())
}

async fn validate_cosmetic(
	state: &ApiState,
	player_id: i32,
	slot: &BodySlot,
	cosmetic_id: i32,
) -> Result<(), WebsocketError> {
	use entities::{cosmetic_allowed_slot, player_owned_cosmetic, prelude::*};

	let owned = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player_id))
		.filter(player_owned_cosmetic::Column::CosmeticId.eq(cosmetic_id))
		.one(&state.database)
		.await?
		.is_some();
	if !owned {
		return Err(WebsocketError::UnownedCosmetic(cosmetic_id));
	}

	let allowed = CosmeticAllowedSlot::find()
		.filter(cosmetic_allowed_slot::Column::CosmeticId.eq(cosmetic_id))
		.filter(cosmetic_allowed_slot::Column::Slot.eq(slot.clone()))
		.one(&state.database)
		.await?
		.is_some();
	if !allowed {
		return Err(WebsocketError::InvalidSlot {
			slot: slot.clone(),
			cosmetic_id,
		});
	}

	Ok(())
}

pub(super) async fn validate_emote(
	state: &ApiState,
	player_id: i32,
	emote_id: i32,
) -> Result<Option<EmoteInfo>, WebsocketError> {
	use entities::{
		cosmetic, player_owned_cosmetic, prelude::*, sea_orm_active_enums::CosmeticType,
	};

	let Some((_, Some(emote))) = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player_id))
		.filter(player_owned_cosmetic::Column::CosmeticId.eq(emote_id))
		.find_also_related(Cosmetic)
		.filter(cosmetic::Column::Type.eq(CosmeticType::Emote))
		.one(&state.database)
		.await?
	else {
		return Err(WebsocketError::UnownedEmote(emote_id));
	};

	let asset = match emote.asset_id {
		Some(asset_id) => Asset::find_by_id(asset_id).one(&state.database).await?,
		None => None,
	};
	match EmoteInfo::from_db_model(
		&emote,
		asset.as_ref(),
		state.asset_cache.clone(),
		state.s3_bucket.clone(),
		&state.s3_public_url,
	)
	.await
	{
		Ok(definition) => {
			remember_emote_definition(&state.realtime, definition.clone()).await;
			Ok(Some(definition))
		}
		Err(error) => {
			warn!("Could not describe emote {emote_id} for its watchers: {error}");
			Ok(None)
		}
	}
}

pub(super) async fn remember_emote_definition(
	realtime: &RealtimeState,
	definition: EmoteInfo,
) {
	realtime
		.emote_definitions
		.write()
		.await
		.insert(definition.id, definition);
}

pub(super) async fn emote_definitions_for(
	realtime: &RealtimeState,
	active_emotes: &HashMap<Uuid, i32>,
	revisions: HashMap<Uuid, (u64, EmoteInfo)>,
) -> (HashMap<i32, EmoteInfo>, HashMap<Uuid, EmoteInfo>) {
	let mut newest: HashMap<i32, (u64, EmoteInfo)> = HashMap::new();
	for (generation, definition) in revisions.values() {
		let replace = newest
			.get(&definition.id)
			.is_none_or(|(newest, _)| generation > newest);
		if replace {
			newest.insert(definition.id, (*generation, definition.clone()));
		}
	}
	let mut by_id: HashMap<i32, EmoteInfo> = newest
		.into_iter()
		.map(|(emote_id, (_, definition))| (emote_id, definition))
		.collect();
	{
		let definitions = realtime.emote_definitions.read().await;
		for emote_id in active_emotes.values() {
			if !by_id.contains_key(emote_id)
				&& let Some(definition) = definitions.get(emote_id)
			{
				by_id.insert(*emote_id, definition.clone());
			}
		}
	}
	let by_player = revisions
		.into_iter()
		.filter(|(_, (_, definition))| by_id.get(&definition.id) != Some(definition))
		.map(|(player, (_, definition))| (player, definition))
		.collect();
	(by_id, by_player)
}

async fn load_runtime(
	state: &ApiState,
	player: &entities::user::Model,
) -> Result<PlayerRuntimeState, WebsocketError> {
	state.equipment_persist.flushed().await;
	state.particle_color_persist.flushed().await;

	// The row loaded at handshake can predate a status change made since.
	// shortcut: narrows the race to this read, not closed; lock per player if it ever matters.
	let user = entities::prelude::User::find_by_id(player.id)
		.one(&state.database)
		.await?
		.unwrap_or_else(|| player.clone());
	let equipped = load_equipped(state, player.id).await?;

	let stored_colors = match colors::load_colors(&state.database, player.id).await {
		Ok(colors) => colors,
		Err(error) => {
			warn!("Unable to load cosmetic colours: {error}");
			StoredColors::default()
		}
	};

	let pet_play = match pet_preferences::load_play(&state.database, player.id).await {
		Ok(play) => play,
		Err(error) => {
			warn!("Unable to load pet preferences: {error}");
			PetPlay::Everyone
		}
	};

	Ok(PlayerRuntimeState {
		equipped,
		active_emote: None,
		active_emote_definition: None,
		particle_color: user.particle_color,
		colors: stored_colors.palettes,
		status_override: user.status_override,
		bonuses: Vec::new(),
		pet_play,
		aura_defaults: stored_colors.aura_defaults,
		live: true,
	})
}

pub(super) struct Attached {
	is_first_connection: bool,
	replaced: Option<PlayerRuntimeState>,
}

pub(super) async fn attach_connection<E>(
	realtime: &RealtimeState,
	connection_id: ConnectionId,
	connection: RealtimeConnection,
	load: impl Future<Output = Result<PlayerRuntimeState, E>>,
) -> Result<Attached, E> {
	let owner = connection.owner;
	let _rules = realtime.slot_rules.read().await;
	let _updates = realtime.player_updates.lock(owner).await;
	let loaded = load.await?;

	realtime
		.connections
		.write()
		.await
		.insert(connection_id, connection);
	let is_first_connection = {
		let mut connections_by_owner = realtime.connections_by_owner.write().await;
		let owner_connections = connections_by_owner.entry(owner).or_default();
		let was_empty = owner_connections.is_empty();
		owner_connections.insert(connection_id);
		was_empty
	};

	let replaced =
		seed_runtime(&mut *realtime.player_runtime.write().await, owner, loaded);
	Ok(Attached {
		is_first_connection,
		replaced,
	})
}

async fn register_connection(
	state: &ApiState,
	player: &entities::user::Model,
	kind: ClientKind,
	access: TokenAccess,
	tx: mpsc::UnboundedSender<ClientBoundPacket>,
) -> Result<ConnectionId, WebsocketError> {
	let player_id = player.id;
	let owner = player.minecraft_uuid;
	let connection_id = Uuid::new_v4();

	let Attached {
		is_first_connection,
		replaced,
	} = attach_connection(
		&state.realtime,
		connection_id,
		RealtimeConnection {
			owner,
			tx,
			subscriptions: HashSet::new(),
			kind,
			access,
		},
		load_runtime(state, player),
	)
	.await?;

	// Playtime is in-game time only. The lock is held across the insert so a
	// game connection closing at the same moment cannot miss this one.
	if kind == ClientKind::Game {
		let mut playtime = state.realtime.playtime.write().await;
		if !playtime.contains_key(&owner) {
			let started_at = Utc::now();
			match open_play_session(state, player_id, started_at).await {
				Ok(session_row_id) => {
					playtime.insert(
						owner,
						PlaytimeSession {
							player_id,
							session_row_id,
							last_accounted_at: started_at,
						},
					);
				}
				// Analytics, not a reason to refuse the connection.
				Err(error) => warn!("Unable to open play session: {error}"),
			}
		}
	}

	if let Some(previous) = replaced {
		announce_replaced_snapshot(state, owner, previous).await;
	}

	rescan_bonuses(state, [owner], Discovery::Everything).await;

	let status_override = state
		.realtime
		.player_runtime
		.read()
		.await
		.get(&owner)
		.and_then(|runtime| runtime.status_override);

	// Notify anyone already watching this player that they came online,
	// unless they chose to stay offline.
	let status = effective_status(true, status_override);
	if is_first_connection && status != PlayerStatus::Offline {
		announce_presence(state, owner, status).await;
	}

	if is_first_connection {
		crate::api::v1::party::on_player_online(state, owner).await;
	}

	Ok(connection_id)
}

fn seed_runtime(
	player_runtime: &mut HashMap<Uuid, PlayerRuntimeState>,
	owner: Uuid,
	mut loaded: PlayerRuntimeState,
) -> Option<PlayerRuntimeState> {
	loaded.live = true;
	match player_runtime.entry(owner) {
		Entry::Occupied(mut entry) if entry.get().live => {
			entry.get_mut().status_override = loaded.status_override;
			None
		}
		Entry::Occupied(mut entry) => {
			loaded.bonuses = entry.get().bonuses.clone();
			Some(mem::replace(entry.get_mut(), loaded))
		}
		Entry::Vacant(entry) => {
			entry.insert(loaded);
			None
		}
	}
}

fn stale_slots(
	previous: &HashMap<BodySlot, i32>,
	current: &HashMap<BodySlot, i32>,
) -> Vec<(BodySlot, Option<i32>)> {
	previous
		.keys()
		.chain(current.keys())
		.collect::<HashSet<_>>()
		.into_iter()
		.filter(|slot| previous.get(*slot) != current.get(*slot))
		.map(|slot| (slot.clone(), current.get(slot).copied()))
		.collect()
}

async fn announce_replaced_snapshot(
	state: &ApiState,
	owner: Uuid,
	previous: PlayerRuntimeState,
) {
	let current = state
		.realtime
		.player_runtime
		.read()
		.await
		.get(&owner)
		.map(|runtime| {
			(
				runtime.equipped.clone(),
				runtime.particle_color,
				runtime.pet_play,
			)
		});
	let Some((equipped, particle_color, pet_play)) = current else {
		return;
	};

	if previous.particle_color != particle_color {
		announce_legacy_particle_color(state, owner, particle_color, false).await;
	}
	for (slot, cosmetic_id) in stale_slots(&previous.equipped, &equipped) {
		announce_equipment_change(state, owner, slot, cosmetic_id, false).await;
	}
	if previous.pet_play != pet_play {
		let friends = pet_friends_of(state, owner, pet_play).await;
		announce_pet_play(&state.realtime, owner, pet_play, &friends).await;
	}
}

async fn open_play_session(
	state: &ApiState,
	player_id: i32,
	started_at: DateTime<Utc>,
) -> Result<i64, sea_orm::DbErr> {
	use entities::{play_session, prelude::*};
	use sea_orm::{ActiveValue, EntityTrait};

	let session = PlaySession::insert(play_session::ActiveModel {
		player_id: ActiveValue::Set(player_id),
		started_at: ActiveValue::Set(started_at.into()),
		last_heartbeat_at: ActiveValue::Set(started_at.into()),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	Ok(session.id)
}

async fn close_play_session(
	state: &ApiState,
	session_row_id: i64,
	ended_at: DateTime<Utc>,
) -> Result<(), sea_orm::DbErr> {
	use entities::{play_session, prelude::*, sea_orm_active_enums::SessionEndReason};
	use sea_orm::{
		ActiveEnum as _, ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr,
	};

	PlaySession::update_many()
		.col_expr(
			play_session::Column::EndedAt,
			Expr::value(sea_orm::prelude::DateTimeWithTimeZone::from(ended_at)),
		)
		.col_expr(
			play_session::Column::EndReason,
			SessionEndReason::Disconnect.as_enum(),
		)
		.filter(play_session::Column::Id.eq(session_row_id))
		.filter(play_session::Column::EndedAt.is_null())
		.exec(&state.database)
		.await?;

	Ok(())
}

async fn unregister_connection(state: &ApiState, connection_id: ConnectionId) {
	let Some(connection) = state
		.realtime
		.connections
		.write()
		.await
		.remove(&connection_id)
	else {
		return;
	};
	let departed = state.realtime.next_emote_generation();

	let mut game_ended = false;
	if connection.kind == ClientKind::Game {
		let session = {
			let mut playtime = state.realtime.playtime.write().await;
			game_ended = last_game_connection_closed(&state.realtime, &connection).await;
			if game_ended {
				playtime.remove(&connection.owner)
			} else {
				None
			}
		};
		if let Some(session) = session {
			let now = Utc::now();
			if let Err(error) = crate::database::accrue_playtime(
				&state.database,
				session.player_id,
				session.last_accounted_at,
				now,
				true,
			)
			.await
			{
				warn!("Unable to record playtime on disconnect: {error}");
			}

			if let Err(error) =
				close_play_session(state, session.session_row_id, now).await
			{
				warn!("Unable to close play session: {error}");
			}
		}
	}

	let owner_still_connected = detach_connection(
		&state.realtime,
		connection_id,
		connection.owner,
		game_ended,
		departed,
	)
	.await;

	if !owner_still_connected {
		let status_override = state
			.realtime
			.player_runtime
			.read()
			.await
			.get(&connection.owner)
			.and_then(|runtime| runtime.status_override);

		crate::api::v0::sessions::expire_sessions_after_grace(state, connection.owner);
		crate::api::v1::party::on_player_offline(state, connection.owner).await;

		// Notify anyone watching this player that they are now offline, unless
		// they already appeared offline.
		if effective_status(true, status_override) != PlayerStatus::Offline {
			announce_presence(state, connection.owner, PlayerStatus::Offline).await;
		}
	}

	let mut orphaned = {
		let mut watchers = state.realtime.watchers.write().await;
		drop_watchers(&mut watchers, connection_id, connection.subscriptions)
	};

	// The owner's own runtime state is only worth keeping while someone is
	// still watching them; `connections_by_owner` no longer lists them.
	if !owner_still_connected {
		orphaned.push(connection.owner);
	}

	prune_player_runtime(&state.realtime, orphaned).await;
}

pub(super) async fn detach_connection(
	realtime: &RealtimeState,
	connection_id: ConnectionId,
	owner: Uuid,
	game_ended: bool,
	departed: u64,
) -> bool {
	let _updates = realtime.player_updates.lock(owner).await;
	let still_connected = {
		let mut connections_by_owner = realtime.connections_by_owner.write().await;
		if let Some(owner_connections) = connections_by_owner.get_mut(&owner) {
			owner_connections.remove(&connection_id);
			let still_connected = !owner_connections.is_empty();
			if !still_connected {
				connections_by_owner.remove(&owner);
				let mut player_runtime = realtime.player_runtime.write().await;
				if let Some(runtime) = player_runtime.get_mut(&owner) {
					runtime.live = false;
				}
			}
			still_connected
		} else {
			false
		}
	};

	if game_ended || !still_connected {
		end_emote_on_disconnect(realtime, owner, departed).await;
	}

	still_connected
}

pub(super) async fn last_game_connection_closed(
	realtime: &RealtimeState,
	closed: &RealtimeConnection,
) -> bool {
	closed.kind == ClientKind::Game
		&& !realtime
			.connections
			.read()
			.await
			.values()
			.any(|other| other.owner == closed.owner && other.kind == ClientKind::Game)
}

pub(super) async fn watch_players(
	realtime: &RealtimeState,
	connection_id: ConnectionId,
	players: Vec<Uuid>,
) -> Option<(Vec<Uuid>, Vec<Uuid>)> {
	let mut seen = HashSet::new();
	let requested = players
		.into_iter()
		.filter(|player| seen.insert(*player))
		.collect::<Vec<_>>();

	let (newly_subscribed, rejected) = {
		let mut connections = realtime.connections.write().await;
		let connection = connections.get_mut(&connection_id)?;

		let mut newly_subscribed = Vec::new();
		let mut rejected = Vec::new();
		for player in requested {
			if is_fake_player(&player) {
				rejected.push(player);
				continue;
			}
			if connection.subscriptions.contains(&player) {
				continue;
			}
			if connection.subscriptions.len() >= MAX_PLAYER_SUBSCRIPTIONS {
				rejected.push(player);
				continue;
			}
			connection.subscriptions.insert(player);
			newly_subscribed.push(player);
		}
		(newly_subscribed, rejected)
	};

	if !newly_subscribed.is_empty() {
		let mut watchers = realtime.watchers.write().await;
		for player in &newly_subscribed {
			watchers.entry(*player).or_default().insert(connection_id);
		}
	}

	Some((newly_subscribed, rejected))
}

async fn unwatch_players(
	realtime: &RealtimeState,
	connection_id: ConnectionId,
	players: Vec<Uuid>,
) -> Vec<Uuid> {
	let removed = {
		let mut connections = realtime.connections.write().await;
		let Some(connection) = connections.get_mut(&connection_id) else {
			return Vec::new();
		};

		// `remove` returns false on a repeat UUID, so the result is bounded by
		// the subscription cap rather than the request's (unvalidated) length.
		let removed = players
			.into_iter()
			.filter(|player| connection.subscriptions.remove(player))
			.collect::<Vec<_>>();
		shrink_set_if_sparse(&mut connection.subscriptions);
		removed
	};

	let mut watchers = realtime.watchers.write().await;
	drop_watchers(&mut watchers, connection_id, removed)
}

pub(super) async fn unwatch_on_failure<T, E>(
	realtime: &RealtimeState,
	connection_id: ConnectionId,
	newly_subscribed: Vec<Uuid>,
	snapshot: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
	let result = snapshot.await;
	if result.is_err() && !newly_subscribed.is_empty() {
		let orphaned = unwatch_players(realtime, connection_id, newly_subscribed).await;
		prune_player_runtime(realtime, orphaned).await;
	}
	result
}

pub(super) fn snapshot_players(
	requester: Uuid,
	requested_self: bool,
	newly_subscribed: &[Uuid],
	rejected: &[Uuid],
) -> Vec<Uuid> {
	let mut players = newly_subscribed.to_vec();
	if requested_self && !players.contains(&requester) && !rejected.contains(&requester) {
		players.push(requester);
	}
	players
}

pub(super) fn sends_saved_colors(requester: Uuid, snapshot_players: &[Uuid]) -> bool {
	snapshot_players.contains(&requester)
}

async fn subscribe(
	state: &ApiState,
	requester: &entities::user::Model,
	connection_id: ConnectionId,
	players: Vec<Uuid>,
	request_id: Option<u64>,
	received_at: i64,
) -> Result<ClientBoundPacket, WebsocketError> {
	let requested_self = players.contains(&requester.minecraft_uuid);
	let Some((newly_subscribed, rejected)) =
		watch_players(&state.realtime, connection_id, players).await
	else {
		return Ok(empty_snapshot(Vec::new(), request_id, received_at));
	};

	if !rejected.is_empty() {
		warn!(
			"Connection {connection_id} rejected {} subscription(s) (cap {MAX_PLAYER_SUBSCRIPTIONS})",
			rejected.len()
		);
	}

	let players = snapshot_players(
		requester.minecraft_uuid,
		requested_self,
		&newly_subscribed,
		&rejected,
	);
	if players.is_empty() {
		return Ok(empty_snapshot(rejected, request_id, received_at));
	}
	let subscribed = subscriptions_of(&state.realtime, connection_id).await;

	unwatch_on_failure(
		&state.realtime,
		connection_id,
		newly_subscribed,
		under_slot_rules(
			&state.realtime,
			snapshot_of(
				state,
				requester,
				&players,
				&subscribed,
				rejected,
				request_id,
				received_at,
			),
		),
	)
	.await
}

async fn subscriptions_of(realtime: &RealtimeState, connection_id: ConnectionId) -> HashSet<Uuid> {
	realtime
		.connections
		.read()
		.await
		.get(&connection_id)
		.map(|connection| connection.subscriptions.clone())
		.unwrap_or_default()
}

async fn snapshot_pet_friends<F>(
	realtime: &RealtimeState,
	watcher: Uuid,
	players: &[Uuid],
	subscribed: &HashSet<Uuid>,
	load_pairs: impl FnOnce(Vec<Uuid>, Vec<Uuid>) -> F,
) -> Result<HashMap<Uuid, Vec<Uuid>>, WebsocketError>
where
	F: Future<Output = Result<Vec<(Uuid, Uuid)>, WebsocketError>>,
{
	let plays = {
		let player_runtime = realtime.player_runtime.read().await;
		players
			.iter()
			.chain(subscribed)
			.filter_map(|player| {
				let play = player_runtime.get(player)?.pet_play;
				(play != PetPlay::Everyone).then_some((*player, play))
			})
			.collect::<HashMap<_, _>>()
	};
	let play_of = |player: Uuid| plays.get(&player).copied().unwrap_or_default();
	let Some(scope) =
		pet_preferences::snapshot_friend_scope(players, subscribed, watcher, play_of)
	else {
		return Ok(HashMap::new());
	};
	let friendships = Friendships::new(load_pairs(scope.focus, scope.others).await?);
	Ok(pet_preferences::snapshot_friends(
		players,
		subscribed,
		watcher,
		play_of,
		&friendships,
	))
}

async fn snapshot_of(
	state: &ApiState,
	requester: &entities::user::Model,
	players: &[Uuid],
	subscribed: &HashSet<Uuid>,
	rejected: Vec<Uuid>,
	request_id: Option<u64>,
	received_at: i64,
) -> Result<ClientBoundPacket, WebsocketError> {
	let RuntimeSnapshot {
		mut equipped,
		active_emotes,
		active_emote_revisions,
		active_emote_starts,
		active_emote_anchors,
		shared_emotes,
		mut particle_colors,
		mut colors,
		mut pet_interactions_off,
		missing,
	} = read_runtime(
		&*state.realtime.player_runtime.read().await,
		players,
	);
	let (emote_definitions, active_emote_definitions) =
		emote_definitions_for(&state.realtime, &active_emotes, active_emote_revisions).await;

	let loaded_equipped = load_equipped_for_players(state, &missing).await?;
	let loaded_particle_colors =
		load_particle_colors_for_players(state, &missing).await?;
	let mut loaded_colors =
		colors::load_colors_for_players(&state.database, &missing).await?;
	let loaded_pet_plays =
		pet_preferences::load_players_play(&state.database, &missing).await?;
	let wants_saved = sends_saved_colors(requester.minecraft_uuid, players);
	let (saved_colors, saved_colors_active) = if wants_saved {
		let palettes = colors::load_owned_palettes(&state.database, requester.id).await?;
		(Some(palettes.colors), Some(palettes.active))
	} else {
		(None, None)
	};
	let catalog = bonuses::catalog(state).await;
	let now = Utc::now();
	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		for (player, equipped) in &loaded_equipped {
			let particle_color = loaded_particle_colors.get(player).copied().flatten();
			let stored = loaded_colors.remove(player).unwrap_or_default();
			let worn = colors::snapshot_colors(equipped, &stored.palettes, particle_color);
			if !worn.is_empty() {
				colors.insert(*player, worn);
			}
			let pet_play = loaded_pet_plays.get(player).copied().unwrap_or_default();
			if pet_play == PetPlay::Off {
				pet_interactions_off.push(*player);
			}
			player_runtime.entry(*player).or_insert_with(|| {
				let bonuses = match catalog.as_ref() {
					Some(catalog) => catalog.active(equipped.values().copied(), now),
					None => {
						state.realtime.bonus_retries.mark(*player);
						Vec::new()
					}
				};
				PlayerRuntimeState {
					equipped: equipped.clone(),
					active_emote: None,
					active_emote_definition: None,
					particle_color,
					colors: stored.palettes,
					status_override: None,
					bonuses,
					pet_play,
					aura_defaults: stored.aura_defaults,
					live: false,
				}
			});
		}
	}
	let bonuses = {
		let player_runtime = state.realtime.player_runtime.read().await;
		players
			.iter()
			.filter_map(|player| {
				let runtime = player_runtime.get(player)?;
				(!runtime.bonuses.is_empty()).then(|| (*player, runtime.bonuses.clone()))
			})
			.collect()
	};
	equipped.extend(loaded_equipped);
	for (player, color) in loaded_particle_colors {
		if let Some(color) = color {
			particle_colors.insert(player, color);
		}
	}

	// A player is a live PolyPlus user if they hold a connection and have not
	// chosen to appear offline.
	let users = {
		let connections_by_owner = state.realtime.connections_by_owner.read().await;
		let player_runtime = state.realtime.player_runtime.read().await;
		players
			.iter()
			.copied()
			.filter(|player| {
				let status_override = player_runtime
					.get(player)
					.and_then(|runtime| runtime.status_override);
				effective_status(
					connections_by_owner.contains_key(player),
					status_override,
				) != PlayerStatus::Offline
			})
			.collect::<Vec<_>>()
	};
	let pet_interactions_friends = snapshot_pet_friends(
		&state.realtime,
		requester.minecraft_uuid,
		players,
		subscribed,
		|focus, others| async move {
			social::friend_pairs_among(&state.database, &focus, &others)
				.await
				.map_err(WebsocketError::from)
		},
	)
	.await?;

	Ok(ClientBoundPacket::SubscriptionSnapshot {
		equipped,
		active_emotes,
		active_emote_starts,
		active_emote_anchors,
		shared_emotes,
		emote_definitions,
		active_emote_definitions,
		particle_colors,
		colors,
		saved_colors,
		saved_colors_active,
		bonuses,
		pet_interactions_off,
		pet_interactions_friends,
		users,
		rejected,
		request_id,
		server_received_time: received_at,
		server_time: Utc::now().timestamp_millis(),
	})
}

async fn unsubscribe(state: &ApiState, connection_id: ConnectionId, players: Vec<Uuid>) {
	let orphaned = unwatch_players(&state.realtime, connection_id, players).await;
	prune_player_runtime(&state.realtime, orphaned).await;
}

pub(crate) async fn send_to_owner(
	state: &ApiState,
	owner: Uuid,
	make_packet: impl FnMut() -> ClientBoundPacket,
) {
	send_to_realtime_owner(&state.realtime, owner, make_packet).await;
}

pub(crate) async fn send_to_realtime_owner(
	realtime: &RealtimeState,
	owner: Uuid,
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	send_to_connections_of(realtime, owner, |_| true, &mut make_packet).await;

	if let Some(listeners) = realtime.delegate_listeners.read().await.get(&owner) {
		for tx in listeners.values() {
			let _ = tx.send(make_packet());
		}
	}
}

pub(crate) async fn send_to_owner_connections(
	realtime: &RealtimeState,
	owner: Uuid,
	make_packet: impl FnMut() -> ClientBoundPacket,
) {
	send_to_connections_of(realtime, owner, RealtimeConnection::acts_as_owner, make_packet)
		.await;
}

async fn send_to_connections_of(
	realtime: &RealtimeState,
	owner: Uuid,
	include: impl Fn(&RealtimeConnection) -> bool,
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) -> bool {
	let connection_ids = realtime
		.connections_by_owner
		.read()
		.await
		.get(&owner)
		.map(|connections| connections.iter().copied().collect::<Vec<_>>())
		.unwrap_or_default();

	let connections = realtime.connections.read().await;
	let mut delivered = false;
	for connection_id in connection_ids {
		if let Some(connection) = connections.get(&connection_id)
			&& include(connection)
		{
			delivered |= connection.tx.send(make_packet()).is_ok();
		}
	}
	delivered
}

/// Takes revoked cosmetics off whoever has them equipped, both in the live
/// state watchers see and in the database.
pub(crate) async fn unequip_revoked(state: &ApiState, revoked: &HashMap<Uuid, Vec<i32>>) {
	use entities::{player_equipped_cosmetic, prelude::*, user};
	use sea_orm::Condition;

	let mut players = revoked.keys().copied().collect::<Vec<_>>();
	players.sort_unstable();
	let mut updates = Vec::with_capacity(players.len());
	for player in players {
		updates.push(state.realtime.player_updates.lock(player).await);
	}

	let mut cleared: HashSet<(Uuid, BodySlot)> = HashSet::new();
	{
		let mut runtime = state.realtime.player_runtime.write().await;
		for (player, ids) in revoked {
			let Some(player_state) = runtime.get_mut(player) else {
				continue;
			};
			player_state.equipped.retain(|slot, id| {
				let keep = !ids.contains(id);
				if !keep {
					cleared.insert((*player, slot.clone()));
				}
				keep
			});
		}
	}

	let persisted = async {
		let users = User::find()
			.filter(user::Column::MinecraftUuid.is_in(revoked.keys().copied()))
			.all(&state.database)
			.await?;
		if users.is_empty() {
			return Ok(Vec::new());
		}

		let owned_by = |user: &user::Model| revoked[&user.minecraft_uuid].iter().copied();
		let removed = PlayerEquippedCosmetic::delete_many()
			.filter(users.iter().fold(Condition::any(), |any, user| {
				any.add(
					Condition::all()
						.add(player_equipped_cosmetic::Column::PlayerId.eq(user.id))
						.add(
							player_equipped_cosmetic::Column::CosmeticId
								.is_in(owned_by(user)),
						),
				)
			}))
			.exec_with_returning(&state.database)
			.await?;

		Ok::<_, sea_orm::DbErr>(
			removed
				.into_iter()
				.filter_map(|row| {
					let user = users.iter().find(|user| user.id == row.player_id)?;
					Some((user.minecraft_uuid, row.slot))
				})
				.collect(),
		)
	}
	.await;
	match persisted {
		Ok(rows) => cleared.extend(rows),
		Err(error) => warn!("Unable to unequip revoked cosmetics: {error}"),
	}

	for (player, slot) in cleared {
		let packet = || ClientBoundPacket::PlayerCosmeticEquipped {
			player,
			slot: slot.clone(),
			cosmetic_id: None,
		};
		broadcast_to_watchers(&state.realtime, player, packet).await;
		send_to_owner(state, player, packet).await;
	}
	drop(updates);

	refresh_bonuses(state, revoked.keys().copied()).await;
}

pub(crate) async fn change_allowed_slots<T, E>(
	state: &ApiState,
	cosmetic_id: i32,
	allowed: &[BodySlot],
	change: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
	let (value, players) =
		reslot_under_rules(&state.realtime, cosmetic_id, allowed, change).await?;
	if !players.is_empty() {
		refresh_bonuses(state, players).await;
	}
	Ok(value)
}

async fn reslot_under_rules<T, E>(
	realtime: &RealtimeState,
	cosmetic_id: i32,
	allowed: &[BodySlot],
	change: impl Future<Output = Result<T, E>>,
) -> Result<(T, Vec<Uuid>), E> {
	let _rules = realtime.slot_rules.write().await;
	let value = change.await?;
	let players = clear_disallowed_slots(realtime, cosmetic_id, allowed).await;
	Ok((value, players))
}

pub(crate) async fn under_slot_rules<T>(
	realtime: &RealtimeState,
	work: impl Future<Output = T>,
) -> T {
	let _rules = realtime.slot_rules.read().await;
	work.await
}

async fn equip_under_rules<E>(
	realtime: &RealtimeState,
	validate: impl Future<Output = Result<(), E>>,
	publish: impl Future<Output = ()>,
) -> Result<(), E> {
	let _rules = realtime.slot_rules.read().await;
	validate.await?;
	publish.await;
	Ok(())
}

async fn clear_disallowed_slots(
	realtime: &RealtimeState,
	cosmetic_id: i32,
	allowed: &[BodySlot],
) -> Vec<Uuid> {
	let outside = |slot: &BodySlot, id: &i32| *id == cosmetic_id && !allowed.contains(slot);
	let mut players = realtime
		.player_runtime
		.read()
		.await
		.iter()
		.filter(|(_, runtime)| runtime.equipped.iter().any(|(slot, id)| outside(slot, id)))
		.map(|(player, _)| *player)
		.collect::<Vec<_>>();
	if players.is_empty() {
		return Vec::new();
	}
	players.sort_unstable();

	let mut updates = Vec::with_capacity(players.len());
	for player in &players {
		updates.push(realtime.player_updates.lock(*player).await);
	}

	let mut cleared = Vec::new();
	{
		let mut runtime = realtime.player_runtime.write().await;
		for player in &players {
			let Some(player_state) = runtime.get_mut(player) else {
				continue;
			};
			player_state.equipped.retain(|slot, id| {
				let keep = !outside(slot, id);
				if !keep {
					cleared.push((*player, slot.clone()));
				}
				keep
			});
		}
	}

	for (player, slot) in &cleared {
		let packet = || ClientBoundPacket::PlayerCosmeticEquipped {
			player: *player,
			slot: slot.clone(),
			cosmetic_id: None,
		};
		broadcast_to_watchers(realtime, *player, packet).await;
		send_to_realtime_owner(realtime, *player, packet).await;
	}
	drop(updates);

	players.retain(|player| cleared.iter().any(|(cleared, _)| cleared == player));
	players
}

async fn publish_equipment_change(
	state: &ApiState,
	player: Uuid,
	slot: BodySlot,
	cosmetic_id: Option<i32>,
) {
	let updates = state.realtime.player_updates.lock(player).await;
	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		let runtime = player_runtime.entry(player).or_default();
		if let Some(cosmetic_id) = cosmetic_id {
			runtime.equipped.insert(slot.clone(), cosmetic_id);
		} else {
			runtime.equipped.remove(&slot);
		}
		state.equipment_persist.enqueue(EquipmentPersistence {
			player,
			slot: slot.clone(),
			cosmetic_id,
		});
	}
	if slot == BodySlot::Aura && cosmetic_id.is_some() {
		adopt_aura_color(state, player, &updates).await;
	}

	announce_equipment_change(state, player, slot, cosmetic_id, false).await;
	bonuses::resolve_transition(state, player, &updates).await;
}

pub(crate) async fn adopt_aura_color(
	state: &ApiState,
	player: Uuid,
	_updates: &OwnedMutexGuard<()>,
) {
	let adopted = {
		let mut player_runtime = state.realtime.player_runtime.write().await;
		player_runtime.get_mut(&player).map(colors::adopt_worn_aura)
	};
	let adopted = match adopted {
		Some(adopted) => adopted,
		None => match stored_aura_adoption(state, player).await {
			Ok(adopted) => adopted,
			Err(error) => {
				warn!("Unable to adopt the saved aura colour: {error}");
				None
			}
		},
	};

	if let Some(color) = adopted {
		state
			.particle_color_persist
			.enqueue(ParticleColorPersistence { player, color });
		announce_legacy_particle_color(state, player, color, true).await;
	}
}

async fn stored_aura_adoption(
	state: &ApiState,
	player: Uuid,
) -> Result<Option<Option<i32>>, sea_orm::DbErr> {
	use entities::{player_equipped_cosmetic, prelude::*, user};

	state.equipment_persist.flushed().await;
	state.particle_color_persist.flushed().await;
	let Some(user) = User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.one(&state.database)
		.await?
	else {
		return Ok(None);
	};
	let aura = PlayerEquippedCosmetic::find()
		.filter(player_equipped_cosmetic::Column::PlayerId.eq(user.id))
		.filter(player_equipped_cosmetic::Column::Slot.eq(BodySlot::Aura))
		.one(&state.database)
		.await?
		.map(|equipment| equipment.cosmetic_id);
	let stored = colors::load_colors(&state.database, user.id).await?;

	Ok(colors::adopted_color(
		user.particle_color,
		aura,
		&stored.palettes,
		&stored.aura_defaults,
	))
}

pub(crate) async fn announce_equipment_change(
	state: &ApiState,
	player: Uuid,
	slot: BodySlot,
	cosmetic_id: Option<i32>,
	notify_owner: bool,
) {
	let packet = || ClientBoundPacket::PlayerCosmeticEquipped {
		player,
		slot: slot.clone(),
		cosmetic_id,
	};
	broadcast_to_watchers(&state.realtime, player, packet).await;
	if notify_owner {
		send_to_owner(state, player, packet).await;
	}

	let Some(cosmetic_id) = cosmetic_id else {
		return;
	};
	let worn = runtime_worn_colors(state, player, &slot, cosmetic_id).await;
	if worn.is_some() || slot == BodySlot::Aura {
		announce_cosmetic_colors(state, player, cosmetic_id, worn, notify_owner).await;
	}
}

async fn runtime_worn_colors(
	state: &ApiState,
	player: Uuid,
	slot: &BodySlot,
	cosmetic_id: i32,
) -> Option<SlotColors> {
	let player_runtime = state.realtime.player_runtime.read().await;
	let runtime = player_runtime.get(&player)?;
	colors::worn_colors(slot, cosmetic_id, &runtime.colors, runtime.particle_color)
}

#[derive(Debug, Default)]
struct Superseded {
	particle_color: Option<Supersession<Uuid>>,
	slots: Vec<Supersession<(Uuid, BodySlot)>>,
}

impl Superseded {
	fn confirm(self) {
		if let Some(particle_color) = self.particle_color {
			particle_color.confirm();
		}
		for slot in self.slots {
			slot.confirm();
		}
	}
}

fn supersede_particle_color(state: &ApiState, player: Uuid) -> Superseded {
	Superseded {
		particle_color: state.particle_color_persist.supersede(player),
		slots: Vec::new(),
	}
}

fn supersede_look(state: &ApiState, player: Uuid) -> Superseded {
	let mut superseded = supersede_particle_color(state, player);
	superseded.slots = BodySlot::iter()
		.filter_map(|slot| state.equipment_persist.supersede((player, slot)))
		.collect();
	superseded
}

async fn commit_superseding(
	txn: DatabaseTransaction,
	superseded: Superseded,
) -> Result<(), sea_orm::DbErr> {
	txn.commit().await?;
	superseded.confirm();
	Ok(())
}

pub(crate) async fn commit_particle_color(
	state: &ApiState,
	player: Uuid,
	txn: DatabaseTransaction,
) -> Result<(), sea_orm::DbErr> {
	let superseded = supersede_particle_color(state, player);
	commit_superseding(txn, superseded).await
}

pub(crate) async fn commit_look(
	state: &ApiState,
	player: Uuid,
	txn: DatabaseTransaction,
) -> Result<(), sea_orm::DbErr> {
	let superseded = supersede_look(state, player);
	commit_superseding(txn, superseded).await
}

async fn store_legacy_particle_color(
	txn: &DatabaseTransaction,
	player_id: i32,
	active_outfit_id: Option<i32>,
	color: Option<i32>,
) -> Result<(), sea_orm::DbErr> {
	use entities::{prelude::*, user};
	use sea_orm::sea_query::Expr;

	User::update_many()
		.col_expr(user::Column::ParticleColor, Expr::value(color))
		.filter(user::Column::Id.eq(player_id))
		.exec(txn)
		.await?;
	mirror_active_outfit_particle_color(txn, player_id, active_outfit_id, color).await
}

async fn lock_active_outfit(
	txn: &DatabaseTransaction,
	player: &entities::user::Model,
) -> Result<Option<i32>, sea_orm::DbErr> {
	use entities::prelude::*;
	use sea_orm::QuerySelect;

	Ok(User::find_by_id(player.id)
		.lock_exclusive()
		.one(txn)
		.await?
		.map_or(player.active_outfit_id, |user| user.active_outfit_id))
}

async fn publish_particle_color_change(
	state: &ApiState,
	player: &entities::user::Model,
	color: Option<i32>,
) -> Result<(), WebsocketError> {
	let owner = player.minecraft_uuid;
	let _updates = state.realtime.player_updates.lock(owner).await;
	let aura = state
		.realtime
		.player_runtime
		.read()
		.await
		.get(&owner)
		.and_then(colors::equipped_aura);
	let planned = store_particle_color(state, player, aura, color).await?;

	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		colors::apply_particle_color(
			player_runtime.entry(owner).or_default(),
			color,
			planned.as_ref(),
		);
	}
	announce_particle_color_change(state, owner, color, false).await;
	Ok(())
}

async fn store_particle_color(
	state: &ApiState,
	player: &entities::user::Model,
	aura: Option<i32>,
	color: Option<i32>,
) -> Result<Option<ParticleChange>, sea_orm::DbErr> {
	use sea_orm::TransactionTrait;

	let txn = state.database.begin().await?;
	let active_outfit_id = lock_active_outfit(&txn, player).await?;

	let planned = match aura {
		Some(aura) => {
			let saved = colors::load_palette(&txn, player.id, aura).await?;
			Some(colors::plan_particle_color(aura, saved, color))
		}
		None => None,
	};
	if let Some(planned) = &planned {
		colors::store_colors(
			&txn,
			player.id,
			planned.aura,
			planned.stored.as_ref(),
			true,
			planned.active,
		)
		.await?;
	}

	store_legacy_particle_color(&txn, player.id, active_outfit_id, color).await?;
	commit_particle_color(state, player.minecraft_uuid, txn).await?;
	Ok(planned)
}

async fn announce_legacy_particle_color(
	state: &ApiState,
	player: Uuid,
	color: Option<i32>,
	notify_owner: bool,
) {
	let packet = || ClientBoundPacket::PlayerParticleColorChanged { player, color };
	broadcast_to_watchers(&state.realtime, player, packet).await;
	if notify_owner {
		send_to_owner(state, player, packet).await;
	}
}

pub(crate) async fn announce_particle_color_change(
	state: &ApiState,
	player: Uuid,
	color: Option<i32>,
	notify_owner: bool,
) {
	announce_legacy_particle_color(state, player, color, notify_owner).await;

	let aura = {
		let player_runtime = state.realtime.player_runtime.read().await;
		player_runtime.get(&player).and_then(|runtime| {
			let aura = *runtime.equipped.get(&BodySlot::Aura)?;
			Some((aura, colors::merge_aura(runtime.colors.get(&aura), color)))
		})
	};
	if let Some((aura, worn)) = aura {
		announce_cosmetic_colors(state, player, aura, worn, notify_owner).await;
	}
}

async fn announce_cosmetic_colors(
	state: &ApiState,
	player: Uuid,
	cosmetic_id: i32,
	colors: Option<SlotColors>,
	notify_owner: bool,
) {
	let packet = || ClientBoundPacket::PlayerColorsChanged {
		player,
		cosmetic_id,
		colors: colors.clone(),
	};
	broadcast_to_watchers(&state.realtime, player, packet).await;
	if notify_owner {
		send_to_owner(state, player, packet).await;
	}
}

async fn set_cosmetic_colors(
	state: &ApiState,
	player: &entities::user::Model,
	cosmetic_id: i32,
	requested: Option<SlotColors>,
	active: bool,
) -> Result<ColorChange, WebsocketError> {
	let cosmetic =
		colors::owned_cosmetic(&state.database, player.id, cosmetic_id).await?;
	let declared = match (&cosmetic, &requested) {
		(Some(cosmetic), Some(_)) => {
			colors::slots_for_asset(state, cosmetic.asset_id, &cosmetic.r#type).await?
		}
		_ => Vec::new(),
	};
	let change = colors::plan_change(
		cosmetic_id,
		cosmetic.as_ref(),
		|cosmetic| colors::custom_colors_allowed(player, cosmetic),
		requested,
		active,
		declared,
	)?;

	let owner = player.minecraft_uuid;
	let _updates = state.realtime.player_updates.lock(owner).await;
	let planned_alias = state
		.realtime
		.player_runtime
		.read()
		.await
		.get(&owner)
		.and_then(|runtime| colors::particle_alias(runtime, cosmetic_id, &change));
	store_cosmetic_colors(state, player, cosmetic_id, &change, planned_alias).await?;

	let (alias, worn) = {
		let mut player_runtime = state.realtime.player_runtime.write().await;
		let runtime = player_runtime.entry(owner).or_default();
		let alias = colors::apply_change(runtime, cosmetic_id, &change);
		(alias, colors::is_worn(runtime, cosmetic_id))
	};

	if let Some(color) = alias {
		announce_legacy_particle_color(state, owner, color, false).await;
	}
	if worn {
		announce_cosmetic_colors(state, owner, cosmetic_id, change.announced(), false).await;
	} else {
		send_to_owner_connections(&state.realtime, owner, || {
			ClientBoundPacket::PlayerColorsChanged {
				player: owner,
				cosmetic_id,
				colors: change.announced(),
			}
		})
		.await;
	}

	Ok(change)
}

async fn store_cosmetic_colors(
	state: &ApiState,
	player: &entities::user::Model,
	cosmetic_id: i32,
	change: &ColorChange,
	alias: Option<Option<i32>>,
) -> Result<(), sea_orm::DbErr> {
	use sea_orm::TransactionTrait;

	let txn = state.database.begin().await?;
	let active_outfit_id = lock_active_outfit(&txn, player).await?;
	colors::store_colors(
		&txn,
		player.id,
		cosmetic_id,
		change.stored.as_ref(),
		change.is_aura,
		change.active,
	)
	.await?;

	match alias {
		Some(color) => {
			store_legacy_particle_color(&txn, player.id, active_outfit_id, color).await?;
			commit_particle_color(state, player.minecraft_uuid, txn).await
		}
		None => txn.commit().await,
	}
}

fn particle_color_saved_reply(
	request_id: Option<u64>,
	color: Option<i32>,
) -> Option<ClientBoundPacket> {
	request_id.map(|request_id| ClientBoundPacket::ParticleColorSaved { request_id, color })
}

fn cosmetic_colors_saved_reply(
	request_id: Option<u64>,
	cosmetic_id: i32,
	change: ColorChange,
) -> Option<ClientBoundPacket> {
	request_id.map(|request_id| ClientBoundPacket::CosmeticColorsSaved {
		request_id,
		cosmetic_id,
		colors: change.stored,
		active: change.active,
	})
}

async fn announce_pet_play(
	realtime: &RealtimeState,
	player: Uuid,
	play: PetPlay,
	friends: &HashSet<Uuid>,
) {
	let connection_ids = realtime
		.watchers
		.read()
		.await
		.get(&player)
		.map(|watchers| watchers.iter().copied().collect::<Vec<_>>())
		.unwrap_or_default();

	let mut connections = realtime.connections.write().await;
	for connection_id in connection_ids {
		if let Some(connection) = connections.get_mut(&connection_id) {
			let _ = connection.tx.send(ClientBoundPacket::PlayerPetPreferencesChanged {
				player,
				interactions: play.allows_interactions(),
				pet_play: play,
				friends: pet_preferences::watcher_friends(
					player,
					play,
					&connection.subscriptions,
					connection.owner,
					friends,
				),
			});
		}
	}
}

async fn publish_pet_preferences(
	realtime: &RealtimeState,
	player: Uuid,
	play: PetPlay,
	friends: &HashSet<Uuid>,
) {
	realtime
		.player_runtime
		.write()
		.await
		.entry(player)
		.or_default()
		.pet_play = play;
	announce_pet_play(realtime, player, play, friends).await;
}

async fn pet_play_of(realtime: &RealtimeState, player: Uuid) -> PetPlay {
	realtime
		.player_runtime
		.read()
		.await
		.get(&player)
		.map(|runtime| runtime.pet_play)
		.unwrap_or_default()
}

async fn pet_friends_of(state: &ApiState, player: Uuid, play: PetPlay) -> HashSet<Uuid> {
	if play != PetPlay::Friends {
		return HashSet::new();
	}
	match social::friends_of(&state.database, player).await {
		Ok(friends) => friends,
		Err(error) => {
			warn!("Unable to load friends for pet play: {error}");
			HashSet::new()
		}
	}
}

async fn change_pet_preferences<E, F, G>(
	realtime: &RealtimeState,
	owner: Uuid,
	requested: PetPlay,
	store: impl FnOnce(PetPlay) -> F,
	friends: impl FnOnce(PetPlay) -> G,
) -> Result<(), E>
where
	F: Future<Output = Result<(), E>>,
	G: Future<Output = HashSet<Uuid>>,
{
	let _updates = realtime.player_updates.lock(owner).await;
	let current = pet_play_of(realtime, owner).await;
	let Some(play) = pet_preferences::plan_change(current, requested) else {
		return Ok(());
	};

	store(play).await?;
	let friends = friends(play).await;
	publish_pet_preferences(realtime, owner, play, &friends).await;

	Ok(())
}

async fn set_pet_preferences(
	state: &ApiState,
	player: &entities::user::Model,
	requested: PetPlay,
) -> Result<(), WebsocketError> {
	change_pet_preferences(
		&state.realtime,
		player.minecraft_uuid,
		requested,
		|play| async move {
			pet_preferences::store_play(&state.database, player.id, play)
				.await
				.map_err(WebsocketError::from)
		},
		|play| pet_friends_of(state, player.minecraft_uuid, play),
	)
	.await
}

pub(crate) async fn announce_presence(
	state: &ApiState,
	player: Uuid,
	status: PlayerStatus,
) {
	broadcast_to_watchers(&state.realtime, player, || {
		ClientBoundPacket::PlayerPresence {
			player,
			online: status != PlayerStatus::Offline,
			status,
		}
	})
	.await;
}

pub(crate) async fn broadcast_all(
	state: &ApiState,
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	let mut connections = state.realtime.connections.write().await;
	for connection in connections.values_mut() {
		let _ = connection.tx.send(make_packet());
	}
}

pub(crate) async fn broadcast_to_watchers(
	realtime: &RealtimeState,
	player: Uuid,
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	let connection_ids = realtime
		.watchers
		.read()
		.await
		.get(&player)
		.map(|watchers| watchers.iter().copied().collect::<Vec<_>>())
		.unwrap_or_default();

	let mut connections = realtime.connections.write().await;
	for connection_id in connection_ids {
		if let Some(connection) = connections.get_mut(&connection_id) {
			let _ = connection.tx.send(make_packet());
		}
	}
}

pub(super) trait Clock {
	fn now(&self) -> i64;
}

impl Clock for i64 {
	fn now(&self) -> i64 {
		*self
	}
}

pub(super) struct SystemClock;

impl Clock for SystemClock {
	fn now(&self) -> i64 {
		Utc::now().timestamp_millis()
	}
}

pub(super) async fn start_emote(
	realtime: &RealtimeState,
	player: Uuid,
	emote_id: i32,
	emote: Option<EmoteInfo>,
	clock: impl Clock,
	anchor: Option<EmoteAnchor>,
) -> i64 {
	let _transition = realtime.emote_transitions.lock().await;
	let started_at = clock.now();
	let ended = {
		let mut runtime = realtime.player_runtime.write().await;
		let player_state = runtime.entry(player).or_default();
		player_state.active_emote_definition = emote.clone();
		let previous = player_state.active_emote.replace(ActiveEmote {
			emote_id,
			started_at,
			anchor,
			shared: None,
			generation: realtime.next_emote_generation(),
		});
		let ended = previous.and_then(|active| active.shared);
		if let Some(shared) = &ended {
			release(&mut runtime, shared);
		}
		ended
	};
	if let Some(shared) = ended {
		announce_stop(realtime, &shared).await;
	}
	broadcast_to_watchers(realtime, player, || ClientBoundPacket::PlayerEmoteStarted {
		player,
		emote_id,
		start_time: Some(started_at),
		anchor,
		request_id: None,
		emote: emote.clone(),
	})
	.await;
	started_at
}

fn emote_start_reply(
	player: Uuid,
	emote_id: i32,
	started_at: i64,
	anchor: Option<EmoteAnchor>,
	request_id: Option<u64>,
) -> Option<ClientBoundPacket> {
	request_id.map(|request_id| ClientBoundPacket::PlayerEmoteStarted {
		player,
		emote_id,
		start_time: Some(started_at),
		anchor,
		request_id: Some(request_id),
		emote: None,
	})
}

pub(super) async fn stop_emote(
	realtime: &RealtimeState,
	player: Uuid,
	emote_id: Option<i32>,
	start_time: Option<i64>,
) {
	let _transition = realtime.emote_transitions.lock().await;
	let ended = {
		let mut player_runtime = realtime.player_runtime.write().await;
		let runtime = player_runtime.entry(player).or_default();
		let active = runtime.active_emote;
		let stale_id = emote_id
			.is_some_and(|emote_id| active.map(|emote| emote.emote_id) != Some(emote_id));
		let stale_start = start_time
			.is_some_and(|start| active.map(|emote| emote.started_at) != Some(start));
		if stale_id || stale_start {
			return;
		}
		let shared = runtime.active_emote.take().and_then(|active| active.shared);
		if let Some(shared) = &shared {
			release(&mut player_runtime, shared);
		}
		shared
	};
	announce_ended(realtime, player, ended).await;
}

async fn announce_ended(
	realtime: &RealtimeState,
	player: Uuid,
	shared: Option<SharedEmote>,
) {
	match shared {
		Some(shared) => announce_stop(realtime, &shared).await,
		None => {
			broadcast_to_watchers(realtime, player, || {
				ClientBoundPacket::PlayerEmoteStopped { player }
			})
			.await;
		}
	}
}

pub(super) async fn end_emote_on_disconnect(
	realtime: &RealtimeState,
	player: Uuid,
	departed: u64,
) {
	let _transition = realtime.emote_transitions.lock().await;
	let ended = {
		let mut player_runtime = realtime.player_runtime.write().await;
		let ended = player_runtime.get_mut(&player).and_then(|runtime| {
			runtime
				.active_emote
				.take_if(|active| active.generation < departed)
		});
		if let Some(shared) = ended.and_then(|active| active.shared) {
			release(&mut player_runtime, &shared);
		}
		ended
	};
	if let Some(active) = ended {
		announce_ended(realtime, player, active.shared).await;
	}
	cancel_requests_of(realtime, player, departed).await;
}

struct RequestError {
	error: WebsocketError,
	request_id: Option<u64>,
}

impl From<WebsocketError> for RequestError {
	fn from(error: WebsocketError) -> Self {
		Self {
			error,
			request_id: None,
		}
	}
}

fn error_packet(error: WebsocketError, request_id: Option<u64>) -> ClientBoundPacket {
	if error.is_internal() {
		tracing::error!(%error, "websocket request failed");
	}

	ClientBoundPacket::Error { error, request_id }
}

async fn handle_msg(
	socket: &mut WebSocket,
	state: &ApiState,
	player: &entities::user::Model,
	connection_id: ConnectionId,
	msg: Result<Message, axum::Error>,
) -> Result<(), RequestError> {
	let received_at = Utc::now().timestamp_millis();
	let msg = msg.map_err(WebsocketError::from)?;

	// Ignore control/keepalive frames. Ping/Pong carry an opaque payload (Ktor
	// sends a Ping every pingInterval) that is not a serializable request, and
	// Close needs no response.
	if matches!(msg, Message::Close(_) | Message::Ping(_) | Message::Pong(_)) {
		return Ok(());
	}

	let parsed = serde_json::from_slice::<ServerBoundPacket>(&msg.into_data())
		.map_err(WebsocketError::Deserialization)?;
	let request_id = parsed.request_id();

	// Every packet counts, on top of the tighter per-packet limits.
	if state.packet_limit.check(player.id).await.is_some() {
		return Err(RequestError {
			error: WebsocketError::RateLimited,
			request_id,
		});
	}

	handle_packet(socket, state, player, connection_id, parsed, received_at)
		.await
		.map_err(|error| RequestError { error, request_id })
}

async fn handle_packet(
	socket: &mut WebSocket,
	state: &ApiState,
	player: &entities::user::Model,
	connection_id: ConnectionId,
	parsed: ServerBoundPacket,
	received_at: i64,
) -> Result<(), WebsocketError> {
	match parsed {
		ServerBoundPacket::GetActiveCosmetics { players } => {
			enforce_max_players_per_request(&players)?;
			send_packet(
				socket,
				ClientBoundPacket::CosmeticsInfo {
					cosmetics: active_cosmetics(state, players).await?,
				},
			)
			.await?;
		}
		ServerBoundPacket::SubscribePlayers {
			players,
			request_id,
		} => {
			enforce_max_players_per_request(&players)?;
			let snapshot = subscribe(
				state,
				player,
				connection_id,
				players,
				request_id,
				received_at,
			)
			.await?;
			send_packet(socket, snapshot).await?;
		}
		ServerBoundPacket::UnsubscribePlayers { players } => {
			unsubscribe(state, connection_id, players).await;
		}
		ServerBoundPacket::SetEquippedCosmetic { slot, cosmetic_id } => {
			let validate = async {
				match cosmetic_id {
					Some(cosmetic_id) => {
						validate_cosmetic(state, player.id, &slot, cosmetic_id).await
					}
					None => Ok(()),
				}
			};
			equip_under_rules(
				&state.realtime,
				validate,
				publish_equipment_change(state, player.minecraft_uuid, slot.clone(), cosmetic_id),
			)
			.await?;
		}
		ServerBoundPacket::SetParticleColor { color, request_id } => {
			if state.color_limit.check(player.id).await.is_some() {
				return Err(WebsocketError::RateLimited);
			}
			publish_particle_color_change(state, player, color).await?;
			if let Some(reply) = particle_color_saved_reply(request_id, color) {
				send_packet(socket, reply).await?;
			}
		}
		ServerBoundPacket::SetCosmeticColors {
			cosmetic_id,
			colors,
			active,
			request_id,
		} => {
			if state.color_limit.check(player.id).await.is_some() {
				return Err(WebsocketError::RateLimited);
			}
			let change =
				set_cosmetic_colors(state, player, cosmetic_id, colors, active).await?;
			if let Some(reply) = cosmetic_colors_saved_reply(request_id, cosmetic_id, change) {
				send_packet(socket, reply).await?;
			}
		}
		ServerBoundPacket::SetPetPreferences { interactions } => {
			if state.pet_preference_limit.check(player.id).await.is_some() {
				return Err(WebsocketError::RateLimited);
			}
			set_pet_preferences(state, player, interactions.setting()).await?;
		}
		ServerBoundPacket::PlayEmote {
			emote_id,
			anchor,
			request_id,
		} => {
			let anchor = anchor
				.map(|anchor| anchor.checked().ok_or(WebsocketError::InvalidEmoteAnchor))
				.transpose()?;
			let emote = validate_emote(state, player.id, emote_id).await?;

			let started_at = start_emote(
				&state.realtime,
				player.minecraft_uuid,
				emote_id,
				emote,
				SystemClock,
				anchor,
			)
			.await;
			if let Some(reply) = emote_start_reply(
				player.minecraft_uuid,
				emote_id,
				started_at,
				anchor,
				request_id,
			) {
				send_packet(socket, reply).await?;
			}
		}
		ServerBoundPacket::StopEmote {
			emote_id,
			start_time,
		} => {
			stop_emote(&state.realtime, player.minecraft_uuid, emote_id, start_time)
				.await;
		}
		ServerBoundPacket::SetPartyState {
			state: member_state,
		} => {
			member_state
				.validate()
				.map_err(WebsocketError::InvalidPartyState)?;

			let limited = state.party_state_limit.check(connection_id).await.is_some();
			crate::api::v1::party::apply_member_state(
				state,
				player.minecraft_uuid,
				player.id,
				member_state,
				limited,
			)
			.await;
		}
		ServerBoundPacket::StartPlayTogether { destination } => {
			crate::api::v1::party::start_play_together(state, player, destination)
				.await?;
		}
		ServerBoundPacket::CancelPlayTogether { id } => {
			crate::api::v1::party::cancel_play_together(state, player, id).await?;
		}
		ServerBoundPacket::PlayTogetherResponse {
			id,
			response,
			reason,
		} => {
			crate::api::v1::party::respond_to_play_together(
				state, player, id, response, reason,
			)
			.await?;
		}
		ServerBoundPacket::RequestHostedJoin { session_id } => {
			crate::api::v1::party::request_hosted_join(state, player, session_id).await?;
		}
		ServerBoundPacket::HostedJoinResponse { request_id, allow } => {
			crate::api::v1::party::respond_to_hosted_join(
				state, player, request_id, allow,
			)
			.await?;
		}
		ServerBoundPacket::SendPing(request) => {
			crate::api::v1::party::send_ping(state, player, request).await?;
		}
		ServerBoundPacket::ClearPing { id } => {
			crate::api::v1::party::clear_ping(state, player, &id).await?;
		}
		ServerBoundPacket::ReplyPing { id, owner, reply } => {
			crate::api::v1::party::reply_to_ping(state, player, &id, &owner, &reply)
				.await?;
		}
		ServerBoundPacket::SharedEmoteRequest { emote_id, target, .. } => {
			if state.shared_emote_limit.check(player.id).await.is_some() {
				return Err(WebsocketError::RateLimited);
			}
			let gate = DatabaseGate {
				state,
				player_id: player.id,
			};
			let opened = shared_emote::request(
				&state.realtime,
				&gate,
				player.minecraft_uuid,
				target,
				emote_id,
				SystemClock,
			)
			.await?;
			if let Some((request_id, expires_at)) = opened {
				shared_emote::expire_later(&state.realtime, request_id, expires_at);
			}
		}
		ServerBoundPacket::SharedEmoteResponse { request_id, accept } => {
			let accepted = shared_emote::respond(
				&state.realtime,
				player.minecraft_uuid,
				request_id,
				accept,
				Utc::now().timestamp_millis(),
			)
			.await?;
			if let Some((request_id, expires_at)) = accepted {
				shared_emote::expire_later(&state.realtime, request_id, expires_at);
			}
		}
		ServerBoundPacket::SharedEmoteStart { request_id, anchor } => {
			let gate = DatabaseGate {
				state,
				player_id: player.id,
			};
			shared_emote::start(
				&state.realtime,
				&gate,
				player.minecraft_uuid,
				request_id,
				anchor,
				SystemClock,
			)
			.await?;
		}
		ServerBoundPacket::SharedEmoteStop => {
			shared_emote::stop(&state.realtime, player.minecraft_uuid).await;
		}
		ServerBoundPacket::SharedEmoteCancel { request_id } => {
			shared_emote::cancel(
				&state.realtime,
				player.minecraft_uuid,
				request_id,
				Utc::now().timestamp_millis(),
			)
			.await;
		}
	}

	Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct EndpointQuery {
	/// Listen as a player who delegated their chats to the caller. Browsers
	/// can't set `x-polyplus-act-as` on a websocket, so it goes here instead.
	act_as: Option<Uuid>,
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	kind: ClientKind,
	access: TokenAccess,
	Query(query): Query<EndpointQuery>,
	ws: WebSocketUpgrade,
) -> Response<Body> {
	if let Some(uuid) = query.act_as {
		return match crate::api::v0::groups::delegated_owner(&state, access, player.id, uuid).await {
			Ok(owner) => listen_as_delegate(state, owner.minecraft_uuid, ws),
			Err(response) => response,
		};
	}

	// Special Chat staff only watch for new messages; they send over HTTP.
	let listen_only = access == TokenAccess::SpecialChat;
	// Browsers send their token as the `bearer` subprotocol, and drop the
	// connection unless the server picks it back.
	ws.protocols(["bearer"]).on_upgrade(async move |mut socket| {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let connection_id = match register_connection(&state, &player, kind, access, tx).await {
			Ok(connection_id) => connection_id,
			Err(error) => {
				let _ = send_packet(&mut socket, error_packet(error, None)).await;
				return;
			}
		};

		let retired = state.realtime.socket_retirements.watch(connection_id);
		loop {
			let result = tokio::select! {
				() = retired.notified() => break,
				msg = socket.recv() => {
					let Some(msg) = msg else {
						break;
					};
					if listen_only {
						Ok(())
					} else {
						handle_msg(&mut socket, &state, &player, connection_id, msg).await
					}
				}
				packet = rx.recv() => {
					let Some(packet) = packet else {
						break;
					};
					tokio::select! {
						written = write_confirmed(packet, |packet| send_packet(&mut socket, packet)) => {
							written.map_err(RequestError::from)
						}
						() = retired.notified() => break,
					}
				}
			};

			match result {
				Ok(_) => continue,
				Err(RequestError {
					error: WebsocketError::Fatal(_),
					..
				}) => break,
				Err(RequestError { error, request_id }) => {
					let e = error_packet(error, request_id);
					if send_packet(&mut socket, e).await.is_err() {
						break;
					};
				}
			}
		}

		rx.close();
		drop(rx);
		state.realtime.socket_retirements.forget(connection_id);
		unregister_connection(&state, connection_id).await;
	})
}

/// Forwards `owner`'s packets and ignores anything sent; the delegate acts over HTTP.
fn listen_as_delegate(state: ApiState, owner: Uuid, ws: WebSocketUpgrade) -> Response<Body> {
	ws.protocols(["bearer"])
		.on_upgrade(async move |mut socket| {
			let (tx, mut rx) = mpsc::unbounded_channel();
			let id = Uuid::new_v4();
			let listeners = &state.realtime.delegate_listeners;
			listeners.write().await.entry(owner).or_default().insert(id, tx);

			loop {
				tokio::select! {
					msg = socket.recv() => if msg.is_none_or(|msg| msg.is_err()) { break },
					packet = rx.recv() => {
						let Some(packet) = packet else { break };
						if send_packet(&mut socket, packet).await.is_err() {
							break;
						}
					}
				}
			}

			let mut listeners = listeners.write().await;
			if let Some(set) = listeners.get_mut(&owner) {
				set.remove(&id);
				if set.is_empty() {
					listeners.remove(&owner);
				}
			}
		})
		.into_response()
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{BTreeMap, HashMap, HashSet},
		future::Future,
		sync::{
			Arc, Mutex,
			atomic::{AtomicBool, Ordering},
		},
	};

	use chrono::Utc;
	use entities::sea_orm_active_enums::{BodySlot, PlayerStatus};
	use tokio::{
		sync::{Notify, mpsc, oneshot},
		task::yield_now,
	};
	use uuid::Uuid;

	use super::{
		ActiveEmote, ClientBoundPacket, ConnectionId, EmoteAnchor, PetPlay, PlayerRuntimeState,
		RealtimeConnection, RealtimeState, attach_connection, change_pet_preferences,
		drop_watchers, emote_definitions_for, empty_snapshot, end_emote_on_disconnect,
		equip_under_rules, last_game_connection_closed, pet_play_of, prune_player_runtime,
		read_runtime, remember_emote_definition, reslot_under_rules, seed_runtime,
		send_to_owner_connections, send_to_realtime_owner, shrink_map_if_sparse,
		shrink_set_if_sparse, snapshot_players, stale_slots, stamp_send_time, start_emote,
		stop_emote, under_slot_rules, unwatch_on_failure, watch_players, write_confirmed,
	};
	use crate::api::v0::{
		account::{ClientKind, TokenAccess},
		bonuses::BonusRef,
		cosmetics::{CachedAssetInfo, EmoteInfo},
		websocket::structs::WriteReceipt,
	};

	type Rx = mpsc::UnboundedReceiver<ClientBoundPacket>;

	const LEDGE: EmoteAnchor = EmoteAnchor { x: 10.5, y: 70.0, z: -4.25, yaw: 90.0 };

	fn emote_info(id: i32, hash: &str) -> EmoteInfo {
		let (name, url) = (format!("Emote {id}"), Some(format!("https://objects.example/emotes/{hash}.zip")));
		EmoteInfo { id, name, url, cached_info: CachedAssetInfo { hash: hash.to_owned() } }
	}

	fn connection_with(owner: Uuid, kind: ClientKind, access: TokenAccess) -> (RealtimeConnection, Rx) {
		let (tx, rx) = mpsc::unbounded_channel();
		let subscriptions = HashSet::new();
		(RealtimeConnection { owner, tx, subscriptions, kind, access }, rx)
	}

	fn connection_of(owner: Uuid, kind: ClientKind) -> RealtimeConnection {
		connection_with(owner, kind, TokenAccess::Player).0
	}

	async fn connect(rt: &RealtimeState, owner: Uuid) -> (ConnectionId, Rx) {
		let (connection, rx) = connection_with(owner, ClientKind::Game, TokenAccess::Player);
		let id = Uuid::new_v4();
		rt.connections.write().await.insert(id, connection);
		(id, rx)
	}

	async fn watch_as(rt: &RealtimeState, owner: Uuid, players: &[Uuid]) -> Rx {
		let (id, rx) = connect(rt, owner).await;
		rt.connections.write().await.get_mut(&id).unwrap().subscriptions.extend(players);
		let mut watchers = rt.watchers.write().await;
		for player in players {
			watchers.entry(*player).or_default().insert(id);
		}
		rx
	}

	async fn watch(rt: &RealtimeState, player: Uuid) -> Rx {
		watch_as(rt, Uuid::new_v4(), &[player]).await
	}

	fn drain(rx: &mut Rx) -> Vec<ClientBoundPacket> {
		std::iter::from_fn(|| rx.try_recv().ok()).collect()
	}

	async fn active_emote(rt: &RealtimeState, player: Uuid) -> Option<ActiveEmote> {
		rt.player_runtime.read().await.get(&player)?.active_emote
	}

	async fn anchor_in_snapshot(rt: &RealtimeState, player: Uuid) -> Option<EmoteAnchor> {
		let snapshot = read_runtime(&*rt.player_runtime.read().await, &[player]);
		snapshot.active_emote_anchors.get(&player).copied()
	}

	async fn wear(rt: &RealtimeState, player: Uuid, slots: HashMap<BodySlot, i32>) {
		rt.player_runtime.write().await.entry(player).or_default().equipped.extend(slots);
	}

	async fn settle() {
		for _ in 0..64 {
			yield_now().await;
		}
	}

	fn pause() -> (oneshot::Receiver<()>, oneshot::Sender<()>, impl Future<Output = ()> + Send + 'static) {
		let (reached_tx, reached) = oneshot::channel();
		let (resume, resumed) = oneshot::channel::<()>();
		let hook = async move {
			let _ = reached_tx.send(());
			let _ = resumed.await;
		};
		(reached, resume, hook)
	}

	#[tokio::test]
	async fn emote_definitions_follow_the_revision_each_player_started() {
		let rt = RealtimeState::default();
		let (first, second, idle) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		let (original, replaced) = (emote_info(7, "old"), emote_info(7, "new"));
		remember_emote_definition(&rt, EmoteInfo { name: "Stale".to_owned(), ..original.clone() }).await;
		remember_emote_definition(&rt, emote_info(9, "idle")).await;
		let mut watcher = watch(&rt, first).await;

		start_emote(&rt, first, 7, Some(original.clone()), 1, None).await;
		assert!(matches!(
			watcher.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStarted { emote: Some(emote), .. }) if emote == original
		));
		remember_emote_definition(&rt, replaced.clone()).await;
		start_emote(&rt, second, 7, Some(replaced.clone()), 2, None).await;

		let snapshot = read_runtime(&*rt.player_runtime.read().await, &[first, second, idle]);
		let revisions = snapshot.active_emote_revisions;
		let (definitions, per_player) = emote_definitions_for(&rt, &snapshot.active_emotes, revisions).await;
		assert_eq!(definitions, HashMap::from([(7, replaced)]));
		assert_eq!(per_player, HashMap::from([(first, original)]));
	}

	#[tokio::test]
	async fn state_published_before_a_slot_is_removed_is_cleared_with_it() {
		for racer in ["equip", "outfit or snapshot", "connection"] {
			let rt = RealtimeState::default();
			let (player, kept) = (Uuid::new_v4(), Uuid::new_v4());
			wear(&rt, kept, HashMap::from([(BodySlot::Hat, 5)])).await;
			let mut watcher = watch(&rt, player).await;
			let (reached, resume, hook) = pause();
			let worn = HashMap::from([(BodySlot::Backpack, 5), (BodySlot::Hat, 6)]);
			let task = tokio::spawn({
				let rt = rt.clone();
				async move {
					match racer {
						"equip" => {
							let validate = async {
								hook.await;
								Ok::<_, ()>(())
							};
							equip_under_rules(&rt, validate, wear(&rt, player, worn)).await.unwrap();
						}
						"connection" => {
							let load = async {
								hook.await;
								Ok::<_, ()>(PlayerRuntimeState { equipped: worn, ..Default::default() })
							};
							let connection = connection_of(player, ClientKind::Game);
							assert!(attach_connection(&rt, Uuid::new_v4(), connection, load).await.is_ok());
						}
						_ => {
							under_slot_rules(&rt, async {
								hook.await;
								wear(&rt, player, worn).await;
							})
							.await
						}
					}
				}
			});
			reached.await.unwrap();

			let reslot = tokio::spawn({
				let rt = rt.clone();
				async move { reslot_under_rules(&rt, 5, &[BodySlot::Hat], async { Ok::<_, ()>(()) }).await }
			});
			settle().await;
			assert!(!reslot.is_finished(), "{racer}");
			let _ = resume.send(());
			task.await.unwrap();
			let ((), cleared) = reslot.await.unwrap().unwrap();

			assert_eq!(cleared, vec![player], "{racer}");
			let runtime = rt.player_runtime.read().await;
			assert_eq!(runtime[&player].equipped, HashMap::from([(BodySlot::Hat, 6)]), "{racer}");
			assert_eq!(runtime[&player].live, racer == "connection");
			assert_eq!(runtime[&kept].equipped, HashMap::from([(BodySlot::Hat, 5)]));
			assert!(drain(&mut watcher).iter().any(|packet| matches!(packet,
				ClientBoundPacket::PlayerCosmeticEquipped { slot: BodySlot::Backpack, cosmetic_id: None, .. }
			)), "{racer}");
		}
	}

	#[tokio::test]
	async fn a_watch_cannot_land_between_a_prune_check_and_its_removal() {
		let rt = RealtimeState::default();
		let (viewer, watched) = (Uuid::new_v4(), Uuid::new_v4());
		rt.player_runtime.write().await.insert(watched, PlayerRuntimeState::default());
		let (connection_id, _rx) = connect(&rt, viewer).await;
		let held = rt.player_runtime.read().await;

		let prune = tokio::spawn({
			let rt = rt.clone();
			async move { prune_player_runtime(&rt, vec![watched]).await }
		});
		settle().await;
		let watching = tokio::spawn({
			let rt = rt.clone();
			async move { watch_players(&rt, connection_id, vec![watched]).await }
		});
		settle().await;
		assert!(!watching.is_finished());

		drop(held);
		prune.await.unwrap();
		assert_eq!(watching.await.unwrap(), Some((vec![watched], Vec::new())));
		assert!(!rt.player_runtime.read().await.contains_key(&watched));
		assert!(rt.watchers.read().await[&watched].contains(&connection_id));
	}

	#[tokio::test]
	async fn a_connection_attaching_during_a_prune_keeps_its_live_state() {
		let rt = RealtimeState::default();
		let owner = Uuid::new_v4();
		wear(&rt, owner, HashMap::from([(BodySlot::Hat, 1)])).await;
		let held = rt.player_runtime.read().await;

		let prune = tokio::spawn({
			let rt = rt.clone();
			async move { prune_player_runtime(&rt, vec![owner]).await }
		});
		settle().await;
		let attach = tokio::spawn({
			let rt = rt.clone();
			async move {
				let equipped = HashMap::from([(BodySlot::Hat, 2)]);
				let load = async { Ok::<_, ()>(PlayerRuntimeState { equipped, ..Default::default() }) };
				let connection = connection_of(owner, ClientKind::Game);
				attach_connection(&rt, Uuid::new_v4(), connection, load).await.is_ok()
			}
		});
		settle().await;

		drop(held);
		prune.await.unwrap();
		assert!(attach.await.unwrap());
		let runtime = rt.player_runtime.read().await;
		assert!(runtime[&owner].live);
		assert_eq!(runtime[&owner].equipped, HashMap::from([(BodySlot::Hat, 2)]));
	}

	#[tokio::test]
	async fn private_colours_skip_delegates_and_staff_answering_as_the_owner() {
		let rt = RealtimeState::default();
		let owner = Uuid::new_v4();
		let mut receivers = Vec::new();
		for access in [TokenAccess::Player, TokenAccess::SpecialChat] {
			let (connection, rx) = connection_with(owner, ClientKind::Game, access);
			let id = Uuid::new_v4();
			rt.connections.write().await.insert(id, connection);
			rt.connections_by_owner.write().await.entry(owner).or_default().insert(id);
			receivers.push(rx);
		}
		let (tx, delegate) = mpsc::unbounded_channel();
		rt.delegate_listeners.write().await.insert(owner, HashMap::from([(Uuid::new_v4(), tx)]));
		receivers.push(delegate);
		let packet = || ClientBoundPacket::PlayerColorsChanged { player: owner, cosmetic_id: 5, colors: None };

		send_to_owner_connections(&rt, owner, packet).await;
		let got = receivers.iter_mut().map(|rx| rx.try_recv().is_ok()).collect::<Vec<_>>();
		assert_eq!(got, [true, false, false]);

		send_to_realtime_owner(&rt, owner, packet).await;
		let got = receivers.iter_mut().map(|rx| rx.try_recv().is_ok()).collect::<Vec<_>>();
		assert_eq!(got, [true, true, true]);
	}

	#[test]
	fn snapshots_restamp_only_the_send_time() {
		let before = Utc::now().timestamp_millis();
		let mut snapshot = empty_snapshot(Vec::new(), Some(1), 1_700_000_000_000);
		let ClientBoundPacket::SubscriptionSnapshot { server_time, .. } = &mut snapshot else { unreachable!() };
		assert!(*server_time >= before);
		*server_time = 0;
		stamp_send_time(&mut snapshot);
		let after = Utc::now().timestamp_millis();
		assert!(matches!(snapshot, ClientBoundPacket::SubscriptionSnapshot {
			server_received_time: 1_700_000_000_000, server_time, saved_colors: None, saved_colors_active: None, ..
		} if (before..=after).contains(&server_time)));
	}

	#[tokio::test]
	async fn starting_an_emote_records_and_broadcasts_it() {
		let rt = RealtimeState::default();
		let players: [Uuid; 6] = std::array::from_fn(|_| Uuid::new_v4());
		let [player, plain, idle, quiet, picky, unknown] = players;
		let (connection_id, mut rx) = connect(&rt, player).await;
		watch_players(&rt, connection_id, vec![player]).await;

		start_emote(&rt, player, 7, None, 1_700_000_000_000, Some(LEDGE)).await;
		start_emote(&rt, plain, 8, None, 2, None).await;
		{
			let mut runtime = rt.player_runtime.write().await;
			runtime.entry(idle).or_default();
			runtime.entry(quiet).or_default().pet_play = PetPlay::Off;
			runtime.entry(picky).or_default().pet_play = PetPlay::Friends;
		}

		let anchor = Some(LEDGE);
		let expected = ActiveEmote { emote_id: 7, started_at: 1_700_000_000_000, anchor, shared: None, generation: 1 };
		assert_eq!(active_emote(&rt, player).await, Some(expected));
		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStarted {
				player: sent,
				emote_id: 7,
				start_time: Some(1_700_000_000_000),
				anchor: Some(anchor),
				request_id: None,
				emote: None,
			}) if sent == player && anchor == LEDGE
		));
		let snapshot = read_runtime(&*rt.player_runtime.read().await, &players);
		assert_eq!(snapshot.active_emotes, HashMap::from([(player, 7), (plain, 8)]));
		assert_eq!(snapshot.active_emote_starts, HashMap::from([(player, 1_700_000_000_000), (plain, 2)]));
		assert_eq!(snapshot.active_emote_anchors, HashMap::from([(player, LEDGE)]));
		assert_eq!(snapshot.pet_interactions_off, vec![quiet]);
		assert_eq!(snapshot.missing, vec![unknown]);
		assert!(snapshot.equipped.contains_key(&idle));
	}

	#[tokio::test]
	async fn stops_only_end_the_run_they_name() {
		type Stop = Option<(Option<i32>, Option<i64>)>;
		let cases: [(&[(i32, i64)], Stop, Option<(i32, i64)>); 10] = [
			(&[(7, 1)], Some((Some(7), None)), None),
			(&[(7, 1)], Some((None, None)), None),
			(&[(7, 1), (8, 2)], Some((Some(7), None)), Some((8, 2))),
			(&[(7, 1), (7, 2)], Some((Some(7), Some(1))), Some((7, 2))),
			(&[(7, 1), (7, 2)], Some((Some(7), Some(2))), None),
			(&[(7, 5)], Some((None, Some(4))), Some((7, 5))),
			(&[(7, 5)], Some((None, Some(5))), None),
			(&[], Some((Some(7), None)), None),
			(&[(7, 1)], None, None),
			(&[], None, None),
		];
		for (starts, stop, left) in cases {
			let rt = RealtimeState::default();
			let player = Uuid::new_v4();
			rt.player_runtime.write().await.entry(player).or_default();
			for (index, (emote, at)) in starts.iter().enumerate() {
				start_emote(&rt, player, *emote, None, *at, (index == 0).then_some(LEDGE)).await;
			}
			let mut rx = watch(&rt, player).await;

			match stop {
				Some((emote_id, start_time)) => stop_emote(&rt, player, emote_id, start_time).await,
				None => end_emote_on_disconnect(&rt, player, rt.next_emote_generation()).await,
			}

			let active = active_emote(&rt, player).await;
			let case = format!("{starts:?} {stop:?}");
			assert_eq!(active.map(|active| (active.emote_id, active.started_at)), left, "{case}");
			let anchored = left.filter(|left| Some(left) == starts.first()).map(|_| LEDGE);
			assert_eq!(anchor_in_snapshot(&rt, player).await, anchored, "{case}");
			let stopped = matches!(
				rx.try_recv(),
				Ok(ClientBoundPacket::PlayerEmoteStopped { player: sent }) if sent == player
			);
			assert_eq!(stopped, !starts.is_empty() && left.is_none(), "{case}");
		}
	}

	#[tokio::test]
	async fn subscribing_mid_emote_snapshots_it_once_and_hears_the_stop() {
		let rt = RealtimeState::default();
		let (viewer, dancer) = (Uuid::new_v4(), Uuid::new_v4());
		assert_eq!(watch_players(&rt, Uuid::new_v4(), vec![dancer]).await, None);
		assert!(rt.watchers.read().await.is_empty());
		start_emote(&rt, dancer, 7, None, 1_700_000_000_000, None).await;
		let (connection_id, mut rx) = connect(&rt, viewer).await;

		let (newly, rejected) = watch_players(&rt, connection_id, vec![dancer, dancer]).await.unwrap();
		let snapshot = read_runtime(&*rt.player_runtime.read().await, &newly);
		assert_eq!((newly, rejected), (vec![dancer], Vec::new()));
		assert_eq!(snapshot.active_emotes, HashMap::from([(dancer, 7)]));
		assert_eq!(snapshot.active_emote_starts, HashMap::from([(dancer, 1_700_000_000_000)]));
		let again = watch_players(&rt, connection_id, vec![dancer]).await;
		assert_eq!(again, Some((Vec::new(), Vec::new())));

		stop_emote(&rt, dancer, Some(7), None).await;
		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStopped { player: sent }) if sent == dancer
		));
	}

	async fn change_pets(rt: &RealtimeState, owner: Uuid, requested: PetPlay, stored: &Arc<Mutex<Vec<PetPlay>>>) {
		let stored = stored.clone();
		let store = |play| async move {
			yield_now().await;
			stored.lock().unwrap().push(play);
			Ok::<_, ()>(())
		};
		change_pet_preferences(rt, owner, requested, store, |_| async { HashSet::new() }).await.unwrap();
	}

	#[tokio::test]
	async fn a_pet_preference_change_waits_for_a_running_update() {
		let rt = RealtimeState::default();
		let owner = Uuid::new_v4();
		let stored = Arc::new(Mutex::new(Vec::new()));
		let update = rt.player_updates.lock(owner).await;

		let change = tokio::spawn({
			let (rt, stored) = (rt.clone(), stored.clone());
			async move { change_pets(&rt, owner, PetPlay::Off, &stored).await }
		});
		settle().await;
		assert!(stored.lock().unwrap().is_empty());
		assert_eq!(pet_play_of(&rt, owner).await, PetPlay::Everyone);

		drop(update);
		change.await.unwrap();
		assert_eq!(*stored.lock().unwrap(), vec![PetPlay::Off]);
		assert_eq!(pet_play_of(&rt, owner).await, PetPlay::Off);
	}

	#[tokio::test]
	async fn concurrent_pet_preference_changes_keep_the_latest_choice() {
		let rt = RealtimeState::default();
		let owner = Uuid::new_v4();
		let stored = Arc::new(Mutex::new(Vec::new()));
		change_pets(&rt, owner, PetPlay::Off, &stored).await;
		stored.lock().unwrap().clear();
		let (storing, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
		let first = tokio::spawn({
			let (rt, stored, storing, release) = (rt.clone(), stored.clone(), storing.clone(), release.clone());
			async move {
				let store = |play| async move {
					storing.notify_one();
					release.notified().await;
					stored.lock().unwrap().push(play);
					Ok::<_, ()>(())
				};
				change_pet_preferences(&rt, owner, PetPlay::Friends, store, |_| async { HashSet::new() }).await
			}
		});
		storing.notified().await;

		let second = tokio::spawn({
			let (rt, stored) = (rt.clone(), stored.clone());
			async move { change_pets(&rt, owner, PetPlay::Off, &stored).await }
		});
		settle().await;
		assert!(stored.lock().unwrap().is_empty());

		release.notify_one();
		first.await.unwrap().unwrap();
		second.await.unwrap();
		assert_eq!(*stored.lock().unwrap(), vec![PetPlay::Friends, PetPlay::Off]);
		assert_eq!(pet_play_of(&rt, owner).await, PetPlay::Off);
	}

	#[test]
	fn a_connection_seeds_state_without_losing_newer_live_state() {
		let player = Uuid::new_v4();
		let mut runtime = HashMap::new();
		let from_db = PlayerRuntimeState { particle_color: Some(0x33), pet_play: PetPlay::Off, ..Default::default() };
		assert!(seed_runtime(&mut runtime, player, from_db).is_none());
		assert_eq!((runtime[&player].particle_color, runtime[&player].pet_play), (Some(0x33), PetPlay::Off));
		assert!(runtime[&player].live);

		let live = PlayerRuntimeState {
			equipped: HashMap::from([(BodySlot::Hat, 4)]),
			particle_color: Some(0x11),
			colors: HashMap::from([(4, BTreeMap::from([("fur".to_owned(), 0x22)]))]),
			pet_play: PetPlay::Friends,
			live: true,
			..Default::default()
		};
		let mut runtime = HashMap::from([(player, live.clone())]);
		let offline = PlayerRuntimeState { status_override: Some(PlayerStatus::Offline), ..Default::default() };
		assert!(seed_runtime(&mut runtime, player, offline).is_none());
		let kept = &runtime[&player];
		assert_eq!((&kept.equipped, kept.particle_color, &kept.colors), (&live.equipped, live.particle_color, &live.colors));
		assert_eq!((kept.pet_play, kept.status_override), (PetPlay::Friends, Some(PlayerStatus::Offline)));

		let bonuses = vec![BonusRef { id: 1, url: None, hash: "a".to_owned() }];
		let watched = PlayerRuntimeState {
			equipped: HashMap::from([(BodySlot::Hat, 4), (BodySlot::Cape, 5)]),
			particle_color: Some(0x11),
			bonuses: bonuses.clone(),
			..Default::default()
		};
		let mut runtime = HashMap::from([(player, watched)]);
		let equipped = HashMap::from([(BodySlot::Cape, 5), (BodySlot::Aura, 6)]);
		let loaded = PlayerRuntimeState { equipped: equipped.clone(), ..Default::default() };
		let replaced = seed_runtime(&mut runtime, player, loaded).expect("an offline snapshot is replaced");
		let seeded = &runtime[&player];
		assert!(seeded.live);
		assert_eq!((&seeded.equipped, seeded.particle_color, &seeded.bonuses), (&equipped, None, &bonuses));
		let mut stale = stale_slots(&replaced.equipped, &seeded.equipped);
		stale.sort_by_key(|(slot, _)| format!("{slot:?}"));
		assert_eq!(stale, vec![(BodySlot::Aura, Some(6)), (BodySlot::Hat, None)]);
	}

	#[tokio::test]
	async fn a_connection_loads_its_state_only_after_a_running_update_finishes() {
		let rt = RealtimeState::default();
		let owner = Uuid::new_v4();
		let update = rt.player_updates.lock(owner).await;
		let loaded = Arc::new(AtomicBool::new(false));

		let attach = tokio::spawn({
			let (rt, loaded) = (rt.clone(), loaded.clone());
			async move {
				let load = async move {
					loaded.store(true, Ordering::SeqCst);
					Ok::<_, ()>(PlayerRuntimeState::default())
				};
				let connection = connection_of(owner, ClientKind::Game);
				attach_connection(&rt, Uuid::new_v4(), connection, load).await.is_ok()
			}
		});
		settle().await;
		assert!(!loaded.load(Ordering::SeqCst));
		assert!(rt.connections_by_owner.read().await.is_empty());

		drop(update);
		assert!(attach.await.unwrap());
		assert!(loaded.load(Ordering::SeqCst));
		assert!(rt.player_runtime.read().await[&owner].live);
	}

	#[tokio::test]
	async fn only_the_last_game_connection_closing_ends_the_session() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		for (owner, kind) in [(player, ClientKind::Other), (Uuid::new_v4(), ClientKind::Game)] {
			rt.connections.write().await.insert(Uuid::new_v4(), connection_of(owner, kind));
		}
		assert!(last_game_connection_closed(&rt, &connection_of(player, ClientKind::Game)).await);

		rt.connections.write().await.insert(Uuid::new_v4(), connection_of(player, ClientKind::Game));
		assert!(!last_game_connection_closed(&rt, &connection_of(player, ClientKind::Game)).await);
		assert!(!last_game_connection_closed(&rt, &connection_of(player, ClientKind::Other)).await);
	}

	#[test]
	fn subscribing_to_yourself_always_snapshots_your_account() {
		let owner = Uuid::new_v4();
		let other = Uuid::new_v4();

		assert_eq!(snapshot_players(owner, true, &[], &[]), vec![owner]);
		assert_eq!(snapshot_players(owner, true, &[other], &[]), vec![other, owner]);
		assert_eq!(snapshot_players(owner, true, &[owner, other], &[]), vec![owner, other]);
		assert_eq!(snapshot_players(owner, true, &[], &[owner]), Vec::<Uuid>::new());
		assert_eq!(snapshot_players(owner, false, &[other], &[]), vec![other]);
		assert_eq!(snapshot_players(owner, false, &[], &[]), Vec::<Uuid>::new());
	}

	#[tokio::test]
	async fn only_a_failed_snapshot_undoes_its_new_watches() {
		let rt = RealtimeState::default();
		let (owner, watched) = (Uuid::new_v4(), Uuid::new_v4());
		let (connection_id, _rx) = connect(&rt, owner).await;
		watch_players(&rt, connection_id, vec![watched]).await;

		let (newly, _) = watch_players(&rt, connection_id, vec![owner, watched]).await.unwrap();
		let failed = async { Err::<(), _>("palettes did not load") };
		assert_eq!(unwatch_on_failure(&rt, connection_id, newly, failed).await, Err("palettes did not load"));
		assert!(!rt.watchers.read().await.contains_key(&owner));
		assert!(rt.watchers.read().await[&watched].contains(&connection_id));

		let (newly, _) = watch_players(&rt, connection_id, vec![owner, watched]).await.unwrap();
		assert_eq!(newly, vec![owner]);
		assert_eq!(unwatch_on_failure(&rt, connection_id, newly, async { Ok::<_, ()>(7) }).await, Ok(7));
		assert!(rt.watchers.read().await[&owner].contains(&connection_id));
	}

	#[test]
	fn reports_players_left_without_watchers() {
		let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
		let (first, second): (ConnectionId, ConnectionId) =
			(Uuid::new_v4(), Uuid::new_v4());
		let mut watchers = HashMap::from([
			(alice, HashSet::from([first, second])),
			(bob, HashSet::from([first])),
		]);

		let orphaned = drop_watchers(&mut watchers, first, [alice, bob]);

		assert_eq!(orphaned, vec![bob]);
		assert_eq!(watchers[&alice], HashSet::from([second]));
		assert!(!watchers.contains_key(&bob));
	}

	#[test]
	fn ignores_players_that_were_never_watched() {
		let mut watchers = HashMap::new();

		let orphaned = drop_watchers(&mut watchers, Uuid::new_v4(), [Uuid::new_v4()]);

		assert!(orphaned.is_empty());
		assert!(watchers.is_empty());
	}

	#[test]
	fn reclaims_capacity_only_once_mostly_empty() {
		let mut map = (0..64).map(|key| (key, ())).collect::<HashMap<_, _>>();

		map.retain(|key, _| *key < 48);
		let mostly_full = map.capacity();
		shrink_map_if_sparse(&mut map);
		assert_eq!(
			map.capacity(),
			mostly_full,
			"a mostly full map keeps its capacity"
		);

		map.retain(|key, _| *key < 4);
		let mostly_holes = map.capacity();
		shrink_map_if_sparse(&mut map);
		assert!(map.capacity() < mostly_holes);
		assert_eq!(map.len(), 4);
	}

	#[test]
	fn reclaims_set_capacity_when_drained() {
		let mut set = (0..64).collect::<HashSet<_>>();
		let populated = set.capacity();

		set.clear();
		shrink_set_if_sparse(&mut set);

		assert!(set.capacity() < populated);
	}

	fn discovery() -> (ClientBoundPacket, oneshot::Receiver<()>) {
		let (tx, rx) = oneshot::channel();
		let written = WriteReceipt::on_settled(move |written| {
			if written {
				let _ = tx.send(());
			}
		});
		let packet = ClientBoundPacket::BonusDiscovered {
			bonus_id: 1,
			name: "Night Watch".to_owned(),
			cosmetic_ids: vec![10, 41],
			written,
		};
		(packet, rx)
	}

	#[tokio::test]
	async fn a_discovery_is_confirmed_only_after_the_socket_write_succeeds() {
		let (packet, mut receipt) = discovery();
		let mut pending_while_writing = false;
		let result = write_confirmed(packet, |packet| {
			pending_while_writing = matches!(receipt.try_recv(), Err(oneshot::error::TryRecvError::Empty));
			async move {
				assert!(matches!(packet, ClientBoundPacket::BonusDiscovered { .. }));
				Ok::<_, ()>(())
			}
		})
		.await;
		assert!(result.is_ok() && pending_while_writing);
		assert!(receipt.try_recv().is_ok());

		let (packet, receipt) = discovery();
		assert!(write_confirmed(packet, |_| async { Err(()) }).await.is_err());
		assert!(receipt.await.is_err());
	}
}
