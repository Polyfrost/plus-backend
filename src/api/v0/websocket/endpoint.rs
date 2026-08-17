use std::collections::{HashMap, HashSet};

use aide::{
	axum::{ApiRouter, routing::ApiMethodDocs},
	openapi::Operation,
	transform::TransformOperation,
};
use axum::{
	body::Body,
	extract::{
		State, WebSocketUpgrade,
		ws::{Message, WebSocket},
	},
	routing::get,
};
use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::BodySlot;
use http::{Response, StatusCode};
use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter};
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use crate::api::{
	ApiState,
	state::{
		ConnectionId, EquipmentPersistence, ParticleColorPersistence, PlayerRuntimeState,
		PlaytimeSession, RealtimeConnection,
	},
	v0::{
		account::AuthenticatedPlayer,
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
		particle_colors: HashMap::new(),
		users: Vec::new(),
		rejected,
		request_id,
	}
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
						 request-response model, but that may not always be true.",
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
	tx: mpsc::UnboundedSender<ClientBoundPacket>,
	equipped: HashMap<BodySlot, i32>,
	particle_color: Option<i32>,
) -> ConnectionId {
	let connection_id = Uuid::new_v4();

	state.realtime.connections.write().await.insert(
		connection_id,
		RealtimeConnection {
			owner,
			tx,
			subscriptions: HashSet::new(),
		},
	);
	let is_first_connection = {
		let mut connections_by_owner = state.realtime.connections_by_owner.write().await;
		let owner_connections = connections_by_owner.entry(owner).or_default();
		let was_empty = owner_connections.is_empty();
		owner_connections.insert(connection_id);
		was_empty
	};

	if is_first_connection {
		let started_at = Utc::now();
		match open_play_session(state, player_id, started_at).await {
			Ok(session_row_id) => {
				state.realtime.playtime.write().await.insert(
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

	{
		let mut player_runtime = state.realtime.player_runtime.write().await;
		player_runtime
			.entry(owner)
			.and_modify(|runtime| {
				runtime.equipped = equipped.clone();
				runtime.particle_color = particle_color;
			})
			.or_insert_with(|| PlayerRuntimeState {
				equipped,
				active_emote: None,
				particle_color,
			});
	}

	// Notify anyone already watching this player that they are now online.
	if is_first_connection {
		broadcast_to_watchers(state, owner, || ClientBoundPacket::PlayerPresence {
			player: owner,
			online: true,
		})
		.await;
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
		if let Some(runtime) = state
			.realtime
			.player_runtime
			.write()
			.await
			.get_mut(&connection.owner)
		{
			runtime.active_emote = None;
		}

		let session = state
			.realtime
			.playtime
			.write()
			.await
			.remove(&connection.owner);
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

		crate::api::v0::sessions::expire_sessions_after_grace(state, connection.owner);

		// Notify anyone watching this player that they are now offline.
		broadcast_to_watchers(state, connection.owner, || {
			ClientBoundPacket::PlayerPresence {
				player: connection.owner,
				online: false,
			}
		})
		.await;
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

async fn subscribe(
	state: &ApiState,
	connection_id: ConnectionId,
	players: Vec<Uuid>,
	request_id: Option<u64>,
) -> Result<ClientBoundPacket, WebsocketError> {
	let mut seen = HashSet::new();
	let requested = players
		.into_iter()
		.filter(|player| seen.insert(*player))
		.collect::<Vec<_>>();

	let (newly_subscribed, rejected) = {
		let mut connections = state.realtime.connections.write().await;
		let Some(connection) = connections.get_mut(&connection_id) else {
			return Ok(empty_snapshot(Vec::new(), request_id));
		};

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

	if !rejected.is_empty() {
		warn!(
			"Connection {connection_id} rejected {} subscription(s) (cap {MAX_PLAYER_SUBSCRIPTIONS})",
			rejected.len()
		);
	}

	if newly_subscribed.is_empty() {
		return Ok(empty_snapshot(rejected, request_id));
	}

	{
		let mut watchers = state.realtime.watchers.write().await;
		for player in &newly_subscribed {
			watchers.entry(*player).or_default().insert(connection_id);
		}
	}

	let mut equipped = HashMap::new();
	let mut active_emotes = HashMap::new();
	let mut particle_colors = HashMap::new();
	let mut missing = Vec::new();
	{
		let player_runtime = state.realtime.player_runtime.read().await;
		for player in &newly_subscribed {
			if let Some(runtime) = player_runtime.get(player) {
				equipped.insert(*player, runtime.equipped.clone());
				if let Some(emote_id) = runtime.active_emote {
					active_emotes.insert(*player, emote_id);
				}
				if let Some(color) = runtime.particle_color {
					particle_colors.insert(*player, color);
				}
			} else {
				missing.push(*player);
			}
		}
	}

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
				});
		}
	}
	equipped.extend(loaded_equipped);
	for (player, color) in loaded_particle_colors {
		if let Some(color) = color {
			particle_colors.insert(player, color);
		}
	}

	// A player is a live PolyPlus user if they currently hold a connection.
	let users = {
		let connections_by_owner = state.realtime.connections_by_owner.read().await;
		newly_subscribed
			.iter()
			.copied()
			.filter(|player| connections_by_owner.contains_key(player))
			.collect::<Vec<_>>()
	};

	Ok(ClientBoundPacket::SubscriptionSnapshot {
		equipped,
		active_emotes,
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
	state: &ApiState,
	player: Uuid,
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	let connection_ids = state
		.realtime
		.watchers
		.read()
		.await
		.get(&player)
		.map(|watchers| watchers.iter().copied().collect::<Vec<_>>())
		.unwrap_or_default();

	let mut connections = state.realtime.connections.write().await;
	for connection_id in connection_ids {
		if let Some(connection) = connections.get_mut(&connection_id) {
			let _ = connection.tx.send(make_packet());
		}
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

			{
				let mut player_runtime = state.realtime.player_runtime.write().await;
				let equipment = &mut player_runtime
					.entry(player.minecraft_uuid)
					.or_default()
					.equipped;
				if let Some(cosmetic_id) = cosmetic_id {
					equipment.insert(slot.clone(), cosmetic_id);
				} else {
					equipment.remove(&slot);
				}
			}
			let _ = state.equipment_persist_tx.try_send(EquipmentPersistence {
				player: player.minecraft_uuid,
				slot: slot.clone(),
				cosmetic_id,
			});
			broadcast_to_watchers(state, player.minecraft_uuid, || {
				ClientBoundPacket::PlayerCosmeticEquipped {
					player: player.minecraft_uuid,
					slot: slot.clone(),
					cosmetic_id,
				}
			})
			.await;
		}
		ServerBoundPacket::SetParticleColor { color } => {
			{
				let mut player_runtime = state.realtime.player_runtime.write().await;
				player_runtime
					.entry(player.minecraft_uuid)
					.or_default()
					.particle_color = color;
			}
			let _ = state
				.particle_color_persist_tx
				.try_send(ParticleColorPersistence {
					player: player.minecraft_uuid,
					color,
				});
			broadcast_to_watchers(state, player.minecraft_uuid, || {
				ClientBoundPacket::PlayerParticleColorChanged {
					player: player.minecraft_uuid,
					color,
				}
			})
			.await;
		}
		ServerBoundPacket::PlayEmote { emote_id } => {
			validate_emote(state, player.id, emote_id).await?;

			{
				let mut player_runtime = state.realtime.player_runtime.write().await;
				player_runtime
					.entry(player.minecraft_uuid)
					.or_default()
					.active_emote = Some(emote_id);
			}
			broadcast_to_watchers(state, player.minecraft_uuid, || {
				ClientBoundPacket::PlayerEmoteStarted {
					player: player.minecraft_uuid,
					emote_id,
				}
			})
			.await;
		}
		ServerBoundPacket::StopEmote => {
			{
				let mut player_runtime = state.realtime.player_runtime.write().await;
				player_runtime
					.entry(player.minecraft_uuid)
					.or_default()
					.active_emote = None;
			}
			broadcast_to_watchers(state, player.minecraft_uuid, || {
				ClientBoundPacket::PlayerEmoteStopped {
					player: player.minecraft_uuid,
				}
			})
			.await;
		}
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	ws: WebSocketUpgrade,
) -> Response<Body> {
	ws.on_upgrade(async move |mut socket| {
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
			tx,
			equipped,
			player.particle_color,
		)
		.await;

		loop {
			let result = tokio::select! {
				msg = socket.recv() => {
					let Some(msg) = msg else {
						break;
					};
					handle_msg(&mut socket, &state, &player, connection_id, msg).await
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

#[cfg(test)]
mod tests {
	use std::collections::{HashMap, HashSet};

	use uuid::Uuid;

	use super::{
		ConnectionId, drop_watchers, shrink_map_if_sparse, shrink_set_if_sparse,
	};

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
		let populated = map.capacity();

		map.retain(|key, _| *key < 48);
		shrink_map_if_sparse(&mut map);
		assert_eq!(
			map.capacity(),
			populated,
			"a mostly full map keeps its capacity"
		);

		map.retain(|key, _| *key < 4);
		shrink_map_if_sparse(&mut map);
		assert!(map.capacity() < populated);
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
