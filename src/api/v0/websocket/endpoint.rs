use std::collections::{HashMap, HashSet};

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
use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter};
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use crate::api::{
	ApiState,
	state::{
		ActiveEmote, ConnectionId, EquipmentPersistence, ParticleColorPersistence,
		PlayerRuntimeState, PlaytimeSession, RealtimeConnection, RealtimeState,
		effective_status,
	},
	v0::{
		account::{AuthenticatedPlayer, ClientKind, TokenAccess},
		websocket::structs::{ClientBoundPacket, ServerBoundPacket, WebsocketError},
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

fn is_fake_player(player: &Uuid) -> bool {
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
async fn prune_player_runtime(state: &ApiState, candidates: Vec<Uuid>) {
	if candidates.is_empty() {
		return;
	}

	let orphaned = {
		let watchers = state.realtime.watchers.read().await;
		let connections_by_owner = state.realtime.connections_by_owner.read().await;

		candidates
			.into_iter()
			.filter(|player| {
				!watchers.contains_key(player)
					&& !connections_by_owner.contains_key(player)
			})
			.collect::<Vec<_>>()
	};

	if orphaned.is_empty() {
		return;
	}

	let mut player_runtime = state.realtime.player_runtime.write().await;
	for player in orphaned {
		player_runtime.remove(&player);
	}
	shrink_map_if_sparse(&mut player_runtime);
}

fn empty_snapshot(rejected: Vec<Uuid>, request_id: Option<u64>) -> ClientBoundPacket {
	ClientBoundPacket::SubscriptionSnapshot {
		equipped: HashMap::new(),
		active_emotes: HashMap::new(),
		active_emote_starts: HashMap::new(),
		particle_colors: HashMap::new(),
		users: Vec::new(),
		rejected,
		request_id,
	}
}

#[derive(Debug, Default)]
struct RuntimeSnapshot {
	equipped: HashMap<Uuid, HashMap<BodySlot, i32>>,
	active_emotes: HashMap<Uuid, i32>,
	active_emote_starts: HashMap<Uuid, i64>,
	particle_colors: HashMap<Uuid, i32>,
	missing: Vec<Uuid>,
}

fn read_runtime(
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
			snapshot
				.active_emote_starts
				.insert(*player, emote.started_at);
		}
		if let Some(color) = runtime.particle_color {
			snapshot.particle_colors.insert(*player, color);
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

async fn send_packet(
	socket: &mut WebSocket,
	packet: ClientBoundPacket,
) -> Result<(), WebsocketError> {
	let serialized = serde_json::to_string(&packet)?;
	socket.send(Message::Text(serialized.into())).await?;
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

async fn validate_emote(
	state: &ApiState,
	player_id: i32,
	emote_id: i32,
) -> Result<(), WebsocketError> {
	use entities::{
		cosmetic, player_owned_cosmetic, prelude::*, sea_orm_active_enums::CosmeticType,
	};

	let owned = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player_id))
		.filter(player_owned_cosmetic::Column::CosmeticId.eq(emote_id))
		.inner_join(Cosmetic)
		.filter(cosmetic::Column::Type.eq(CosmeticType::Emote))
		.one(&state.database)
		.await?
		.is_some();
	if !owned {
		return Err(WebsocketError::UnownedEmote(emote_id));
	}

	Ok(())
}

async fn register_connection(
	state: &ApiState,
	player_id: i32,
	owner: Uuid,
	kind: ClientKind,
	tx: mpsc::UnboundedSender<ClientBoundPacket>,
	equipped: HashMap<BodySlot, i32>,
	particle_color: Option<i32>,
	status_override: Option<PlayerStatus>,
) -> ConnectionId {
	let connection_id = Uuid::new_v4();

	state.realtime.connections.write().await.insert(
		connection_id,
		RealtimeConnection {
			owner,
			tx,
			subscriptions: HashSet::new(),
			kind,
		},
	);
	let is_first_connection = {
		let mut connections_by_owner = state.realtime.connections_by_owner.write().await;
		let owner_connections = connections_by_owner.entry(owner).or_default();
		let was_empty = owner_connections.is_empty();
		owner_connections.insert(connection_id);
		was_empty
	};

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

	// The row loaded at handshake can predate a status change made since.
	// shortcut: narrows the race to this read, not closed; lock per player if it ever matters.
	let status_override = match entities::prelude::User::find_by_id(player_id)
		.one(&state.database)
		.await
	{
		Ok(Some(user)) => user.status_override,
		Ok(None) => status_override,
		Err(error) => {
			warn!("Unable to reload status override: {error}");
			status_override
		}
	};

	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		player_runtime
			.entry(owner)
			.and_modify(|runtime| {
				runtime.equipped = equipped.clone();
				runtime.particle_color = particle_color;
				runtime.status_override = status_override;
			})
			.or_insert_with(|| PlayerRuntimeState {
				equipped,
				active_emote: None,
				particle_color,
				status_override,
			});
	}

	// Notify anyone already watching this player that they came online,
	// unless they chose to stay offline.
	let status = effective_status(true, status_override);
	if is_first_connection && status != PlayerStatus::Offline {
		announce_presence(state, owner, status).await;
	}

	if is_first_connection {
		crate::api::v1::party::on_player_online(state, owner).await;
	}

	connection_id
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

	if connection.kind == ClientKind::Game {
		let session = {
			let mut playtime = state.realtime.playtime.write().await;
			let game_still_connected = state
				.realtime
				.connections
				.read()
				.await
				.values()
				.any(|other| other.owner == connection.owner && other.kind == ClientKind::Game);
			if game_still_connected {
				None
			} else {
				playtime.remove(&connection.owner)
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

	let owner_still_connected = {
		let mut connections_by_owner = state.realtime.connections_by_owner.write().await;
		if let Some(owner_connections) = connections_by_owner.get_mut(&connection.owner) {
			owner_connections.remove(&connection_id);
			let still_connected = !owner_connections.is_empty();
			if !still_connected {
				connections_by_owner.remove(&connection.owner);
			}
			still_connected
		} else {
			false
		}
	};

	if !owner_still_connected {
		end_emote_on_disconnect(&state.realtime, connection.owner).await;
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

	prune_player_runtime(state, orphaned).await;
}

async fn watch_players(
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

async fn subscribe(
	state: &ApiState,
	connection_id: ConnectionId,
	players: Vec<Uuid>,
	request_id: Option<u64>,
) -> Result<ClientBoundPacket, WebsocketError> {
	let Some((newly_subscribed, rejected)) =
		watch_players(&state.realtime, connection_id, players).await
	else {
		return Ok(empty_snapshot(Vec::new(), request_id));
	};

	if !rejected.is_empty() {
		warn!(
			"Connection {connection_id} rejected {} subscription(s) (cap {MAX_PLAYER_SUBSCRIPTIONS})",
			rejected.len()
		);
	}

	if newly_subscribed.is_empty() {
		return Ok(empty_snapshot(rejected, request_id));
	}

	let RuntimeSnapshot {
		mut equipped,
		active_emotes,
		active_emote_starts,
		mut particle_colors,
		missing,
	} = read_runtime(
		&*state.realtime.player_runtime.read().await,
		&newly_subscribed,
	);

	let loaded_equipped = load_equipped_for_players(state, &missing).await?;
	let loaded_particle_colors =
		load_particle_colors_for_players(state, &missing).await?;
	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		for (player, equipped) in &loaded_equipped {
			player_runtime
				.entry(*player)
				.or_insert_with(|| PlayerRuntimeState {
					equipped: equipped.clone(),
					active_emote: None,
					particle_color: loaded_particle_colors.get(player).copied().flatten(),
					status_override: None,
				});
		}
	}
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
		newly_subscribed
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

	Ok(ClientBoundPacket::SubscriptionSnapshot {
		equipped,
		active_emotes,
		active_emote_starts,
		particle_colors,
		users,
		rejected,
		request_id,
	})
}

async fn unsubscribe(state: &ApiState, connection_id: ConnectionId, players: Vec<Uuid>) {
	let removed = {
		let mut connections = state.realtime.connections.write().await;
		let Some(connection) = connections.get_mut(&connection_id) else {
			return;
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

	let orphaned = {
		let mut watchers = state.realtime.watchers.write().await;
		drop_watchers(&mut watchers, connection_id, removed)
	};

	prune_player_runtime(state, orphaned).await;
}

pub(crate) async fn send_to_owner(
	state: &ApiState,
	owner: Uuid,
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	let connection_ids = state
		.realtime
		.connections_by_owner
		.read()
		.await
		.get(&owner)
		.map(|connections| connections.iter().copied().collect::<Vec<_>>())
		.unwrap_or_default();

	let mut connections = state.realtime.connections.write().await;
	for connection_id in connection_ids {
		if let Some(connection) = connections.get_mut(&connection_id) {
			let _ = connection.tx.send(make_packet());
		}
	}
	drop(connections);

	if let Some(listeners) = state.realtime.delegate_listeners.read().await.get(&owner) {
		for tx in listeners.values() {
			let _ = tx.send(make_packet());
		}
	}
}

/// Takes revoked cosmetics off whoever has them equipped, both in the live
/// state watchers see and in the database.
pub(crate) async fn unequip_revoked(state: &ApiState, revoked: &HashMap<Uuid, Vec<i32>>) {
	use entities::{player_equipped_cosmetic, prelude::*, user};
	use sea_orm::Condition;

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
}

async fn publish_equipment_change(
	state: &ApiState,
	player: Uuid,
	slot: BodySlot,
	cosmetic_id: Option<i32>,
) {
	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		let equipped = &mut player_runtime.entry(player).or_default().equipped;
		if let Some(cosmetic_id) = cosmetic_id {
			equipped.insert(slot.clone(), cosmetic_id);
		} else {
			equipped.remove(&slot);
		}
		state.equipment_persist.enqueue(EquipmentPersistence {
			player,
			slot: slot.clone(),
			cosmetic_id,
		});
	}

	announce_equipment_change(state, player, slot, cosmetic_id, false).await;
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
}

async fn publish_particle_color_change(
	state: &ApiState,
	player: Uuid,
	color: Option<i32>,
) {
	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		player_runtime.entry(player).or_default().particle_color = color;
		state
			.particle_color_persist
			.enqueue(ParticleColorPersistence { player, color });
	}

	announce_particle_color_change(state, player, color, false).await;
}

pub(crate) async fn announce_particle_color_change(
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

async fn broadcast_to_watchers(
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

async fn start_emote(
	realtime: &RealtimeState,
	player: Uuid,
	emote_id: i32,
	started_at: i64,
) {
	realtime
		.player_runtime
		.write()
		.await
		.entry(player)
		.or_default()
		.active_emote = Some(ActiveEmote {
		emote_id,
		started_at,
	});
	broadcast_to_watchers(realtime, player, || ClientBoundPacket::PlayerEmoteStarted {
		player,
		emote_id,
		start_time: Some(started_at),
	})
	.await;
}

async fn stop_emote(realtime: &RealtimeState, player: Uuid, emote_id: Option<i32>) {
	{
		let mut player_runtime = realtime.player_runtime.write().await;
		let runtime = player_runtime.entry(player).or_default();
		let active_id = runtime.active_emote.map(|emote| emote.emote_id);
		if emote_id.is_some_and(|emote_id| active_id != Some(emote_id)) {
			return;
		}
		runtime.active_emote = None;
	}
	broadcast_to_watchers(realtime, player, || ClientBoundPacket::PlayerEmoteStopped {
		player,
	})
	.await;
}

async fn end_emote_on_disconnect(realtime: &RealtimeState, player: Uuid) {
	let ended = realtime
		.player_runtime
		.write()
		.await
		.get_mut(&player)
		.and_then(|runtime| runtime.active_emote.take())
		.is_some();
	if ended {
		broadcast_to_watchers(realtime, player, || {
			ClientBoundPacket::PlayerEmoteStopped { player }
		})
		.await;
	}
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

	handle_packet(socket, state, player, connection_id, parsed)
		.await
		.map_err(|error| RequestError { error, request_id })
}

async fn handle_packet(
	socket: &mut WebSocket,
	state: &ApiState,
	player: &entities::user::Model,
	connection_id: ConnectionId,
	parsed: ServerBoundPacket,
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
			let snapshot = subscribe(state, connection_id, players, request_id).await?;
			send_packet(socket, snapshot).await?;
		}
		ServerBoundPacket::UnsubscribePlayers { players } => {
			unsubscribe(state, connection_id, players).await;
		}
		ServerBoundPacket::SetEquippedCosmetic { slot, cosmetic_id } => {
			if let Some(cosmetic_id) = cosmetic_id {
				validate_cosmetic(state, player.id, &slot, cosmetic_id).await?;
			}

			publish_equipment_change(state, player.minecraft_uuid, slot, cosmetic_id)
				.await;
		}
		ServerBoundPacket::SetParticleColor { color } => {
			publish_particle_color_change(state, player.minecraft_uuid, color).await;
		}
		ServerBoundPacket::PlayEmote { emote_id } => {
			validate_emote(state, player.id, emote_id).await?;

			start_emote(
				&state.realtime,
				player.minecraft_uuid,
				emote_id,
				Utc::now().timestamp_millis(),
			)
			.await;
		}
		ServerBoundPacket::StopEmote { emote_id } => {
			stop_emote(&state.realtime, player.minecraft_uuid, emote_id).await;
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
		let equipped = match load_equipped(&state, player.id).await {
			Ok(equipped) => equipped,
			Err(error) => {
				let _ = send_packet(&mut socket, error_packet(error, None)).await;
				return;
			}
		};
		let connection_id = register_connection(
			&state,
			player.id,
			player.minecraft_uuid,
			kind,
			tx,
			equipped,
			player.particle_color,
			player.status_override,
		)
		.await;

		loop {
			let result = tokio::select! {
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
					send_packet(&mut socket, packet).await.map_err(RequestError::from)
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
	use std::collections::{HashMap, HashSet};

	use tokio::sync::mpsc;
	use uuid::Uuid;

	use super::{
		ActiveEmote, ClientBoundPacket, ConnectionId, PlayerRuntimeState,
		RealtimeConnection, RealtimeState, drop_watchers, end_emote_on_disconnect,
		read_runtime, shrink_map_if_sparse, shrink_set_if_sparse, start_emote,
		stop_emote, watch_players,
	};
	use crate::api::v0::account::ClientKind;

	async fn watch(
		realtime: &RealtimeState,
		player: Uuid,
	) -> mpsc::UnboundedReceiver<ClientBoundPacket> {
		let (tx, rx) = mpsc::unbounded_channel();
		let connection_id = Uuid::new_v4();
		realtime.connections.write().await.insert(
			connection_id,
			RealtimeConnection {
				owner: Uuid::new_v4(),
				tx,
				subscriptions: HashSet::from([player]),
				kind: ClientKind::Game,
			},
		);
		realtime
			.watchers
			.write()
			.await
			.entry(player)
			.or_default()
			.insert(connection_id);
		rx
	}

	async fn active_emote(realtime: &RealtimeState, player: Uuid) -> Option<ActiveEmote> {
		realtime
			.player_runtime
			.read()
			.await
			.get(&player)
			.and_then(|runtime| runtime.active_emote)
	}

	#[tokio::test]
	async fn records_and_broadcasts_emote_start_time() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		let mut rx = watch(&realtime, player).await;

		start_emote(&realtime, player, 7, 1_700_000_000_000).await;

		assert_eq!(
			active_emote(&realtime, player).await,
			Some(ActiveEmote {
				emote_id: 7,
				started_at: 1_700_000_000_000
			})
		);
		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStarted {
				player: sent,
				emote_id: 7,
				start_time: Some(1_700_000_000_000),
			}) if sent == player
		));
	}

	#[test]
	fn snapshot_carries_emote_start_times() {
		let (dancing, idle, unknown) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		let runtime = HashMap::from([
			(
				dancing,
				PlayerRuntimeState {
					active_emote: Some(ActiveEmote {
						emote_id: 7,
						started_at: 1_700_000_000_000,
					}),
					..Default::default()
				},
			),
			(idle, PlayerRuntimeState::default()),
		]);

		let snapshot = read_runtime(&runtime, &[dancing, idle, unknown]);

		assert_eq!(snapshot.active_emotes, HashMap::from([(dancing, 7)]));
		assert_eq!(
			snapshot.active_emote_starts,
			HashMap::from([(dancing, 1_700_000_000_000)])
		);
		assert_eq!(snapshot.missing, vec![unknown]);
		assert!(snapshot.equipped.contains_key(&idle));
	}

	#[tokio::test]
	async fn stop_with_matching_emote_id_clears() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		start_emote(&realtime, player, 7, 1).await;
		let mut rx = watch(&realtime, player).await;

		stop_emote(&realtime, player, Some(7)).await;

		assert_eq!(active_emote(&realtime, player).await, None);
		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStopped { player: sent }) if sent == player
		));
	}

	#[tokio::test]
	async fn stop_with_stale_emote_id_is_ignored() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		start_emote(&realtime, player, 7, 1).await;
		start_emote(&realtime, player, 8, 2).await;
		let mut rx = watch(&realtime, player).await;

		stop_emote(&realtime, player, Some(7)).await;

		assert_eq!(
			active_emote(&realtime, player).await,
			Some(ActiveEmote {
				emote_id: 8,
				started_at: 2
			})
		);
		assert!(rx.try_recv().is_err());
	}

	#[tokio::test]
	async fn stop_with_emote_id_and_nothing_active_is_ignored() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		let mut rx = watch(&realtime, player).await;

		stop_emote(&realtime, player, Some(7)).await;

		assert_eq!(active_emote(&realtime, player).await, None);
		assert!(rx.try_recv().is_err());
	}

	#[tokio::test]
	async fn stop_without_emote_id_clears() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		start_emote(&realtime, player, 7, 1).await;
		let mut rx = watch(&realtime, player).await;

		stop_emote(&realtime, player, None).await;

		assert_eq!(active_emote(&realtime, player).await, None);
		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStopped { player: sent }) if sent == player
		));
	}

	async fn connect(
		realtime: &RealtimeState,
		owner: Uuid,
	) -> (ConnectionId, mpsc::UnboundedReceiver<ClientBoundPacket>) {
		let (tx, rx) = mpsc::unbounded_channel();
		let connection_id = Uuid::new_v4();
		realtime.connections.write().await.insert(
			connection_id,
			RealtimeConnection {
				owner,
				tx,
				subscriptions: HashSet::new(),
				kind: ClientKind::Game,
			},
		);
		(connection_id, rx)
	}

	#[tokio::test]
	async fn partymate_subscribing_mid_emote_gets_its_start_and_later_stop() {
		let realtime = RealtimeState::default();
		let (viewer, dancer) = (Uuid::new_v4(), Uuid::new_v4());
		start_emote(&realtime, dancer, 7, 1_700_000_000_000).await;
		let (connection_id, mut rx) = connect(&realtime, viewer).await;

		let (newly_subscribed, rejected) =
			watch_players(&realtime, connection_id, vec![dancer, dancer])
				.await
				.expect("connection should exist");
		let snapshot =
			read_runtime(&*realtime.player_runtime.read().await, &newly_subscribed);

		assert_eq!(newly_subscribed, vec![dancer]);
		assert!(rejected.is_empty());
		assert_eq!(snapshot.active_emotes, HashMap::from([(dancer, 7)]));
		assert_eq!(
			snapshot.active_emote_starts,
			HashMap::from([(dancer, 1_700_000_000_000)])
		);

		stop_emote(&realtime, dancer, Some(7)).await;

		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStopped { player: sent }) if sent == dancer
		));
	}

	#[tokio::test]
	async fn resubscribing_reports_nothing_new() {
		let realtime = RealtimeState::default();
		let dancer = Uuid::new_v4();
		let (connection_id, _rx) = connect(&realtime, Uuid::new_v4()).await;

		watch_players(&realtime, connection_id, vec![dancer]).await;
		let again = watch_players(&realtime, connection_id, vec![dancer]).await;

		assert_eq!(again, Some((Vec::new(), Vec::new())));
	}

	#[tokio::test]
	async fn watching_needs_a_live_connection() {
		let realtime = RealtimeState::default();

		let result = watch_players(&realtime, Uuid::new_v4(), vec![Uuid::new_v4()]).await;

		assert_eq!(result, None);
		assert!(realtime.watchers.read().await.is_empty());
	}

	#[tokio::test]
	async fn player_watching_itself_receives_its_own_emote() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		let (connection_id, mut rx) = connect(&realtime, player).await;
		watch_players(&realtime, connection_id, vec![player]).await;

		start_emote(&realtime, player, 7, 1_700_000_000_000).await;

		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStarted {
				player: sent,
				emote_id: 7,
				start_time: Some(1_700_000_000_000),
			}) if sent == player
		));
	}

	#[tokio::test]
	async fn disconnecting_mid_emote_tells_watchers_it_stopped() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		start_emote(&realtime, player, 7, 1).await;
		let mut rx = watch(&realtime, player).await;

		end_emote_on_disconnect(&realtime, player).await;

		assert_eq!(active_emote(&realtime, player).await, None);
		assert!(matches!(
			rx.try_recv(),
			Ok(ClientBoundPacket::PlayerEmoteStopped { player: sent }) if sent == player
		));
	}

	#[tokio::test]
	async fn disconnecting_without_an_emote_sends_nothing() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		realtime
			.player_runtime
			.write()
			.await
			.insert(player, PlayerRuntimeState::default());
		let mut rx = watch(&realtime, player).await;

		end_emote_on_disconnect(&realtime, player).await;

		assert!(rx.try_recv().is_err());
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

		// Alice is still watched by the second connection, bob by nobody.
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
}
