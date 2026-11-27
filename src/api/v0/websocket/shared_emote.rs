use std::{
	collections::{HashMap, HashSet},
	future::{Future, ready},
	time::Duration,
};

use chrono::Utc;
use entities::{prelude::*, sea_orm_active_enums::SharedEmotePrivacy, user};
use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _};
use uuid::Uuid;

use super::{
	endpoint::{
		Clock, broadcast_to_watchers, is_fake_player, send_to_owner_connections,
		validate_emote,
	},
	structs::{
		ClientBoundPacket, EmoteAnchor, SharedEmote, SharedEmoteResult, WebsocketError,
	},
};
use crate::api::{
	ApiState,
	state::{ActiveEmote, ConnectionId, PlayerRuntimeState, RealtimeState},
	v0::{
		account::ClientKind,
		cosmetics::EmoteInfo,
		social::{are_friends, is_blocked_either_way},
	},
};

pub(super) const REQUEST_LIFETIME_MS: i64 = 10_000;
pub(super) const ACCEPTED_LIFETIME_MS: i64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
	Allowed,
	Declined,
}

pub(super) fn admit(
	privacy: SharedEmotePrivacy,
	blocked: bool,
	friends: bool,
) -> Admission {
	if blocked {
		return Admission::Declined;
	}
	match privacy {
		SharedEmotePrivacy::Everyone => Admission::Allowed,
		SharedEmotePrivacy::Friends if friends => Admission::Allowed,
		SharedEmotePrivacy::Friends | SharedEmotePrivacy::Nobody => Admission::Declined,
	}
}

pub(super) trait SharedEmoteGate {
	fn ensure_owns(
		&self,
		emote_id: i32,
	) -> impl Future<Output = Result<Option<EmoteInfo>, WebsocketError>> + Send;

	fn admission(
		&self,
		target: Uuid,
	) -> impl Future<Output = Result<Option<Admission>, WebsocketError>> + Send;
}

pub(super) struct DatabaseGate<'a> {
	pub(super) state: &'a ApiState,
	pub(super) player_id: i32,
}

impl SharedEmoteGate for DatabaseGate<'_> {
	async fn ensure_owns(&self, emote_id: i32) -> Result<Option<EmoteInfo>, WebsocketError> {
		validate_emote(self.state, self.player_id, emote_id).await
	}

	async fn admission(&self, target: Uuid) -> Result<Option<Admission>, WebsocketError> {
		let Some(target) = User::find()
			.filter(user::Column::MinecraftUuid.eq(target))
			.one(&self.state.database)
			.await?
		else {
			return Ok(None);
		};
		let privacy = target.shared_emote_privacy;
		let blocked =
			is_blocked_either_way(self.state, self.player_id, target.id).await?;
		let friends = !blocked
			&& privacy == SharedEmotePrivacy::Friends
			&& are_friends(self.state, self.player_id, target.id).await?;
		Ok(Some(admit(privacy, blocked, friends)))
	}
}

#[derive(Debug, Clone)]
struct OpenRequest {
	id: Uuid,
	emote_id: i32,
	emote: EmoteInfo,
	from: Uuid,
	target: Uuid,
	expires_at: i64,
	accepted: bool,
	generation: u64,
}

impl OpenRequest {
	fn involves(&self, player: Uuid) -> bool {
		self.from == player || self.target == player
	}

	fn pairs(&self, a: Uuid, b: Uuid) -> bool {
		self.involves(a) && self.involves(b)
	}

	fn players(&self) -> [Uuid; 2] {
		[self.from, self.target]
	}
}

#[derive(Debug, Default)]
pub(crate) struct SharedEmoteRequests {
	open: HashMap<Uuid, OpenRequest>,
}

async fn connections_of(realtime: &RealtimeState, player: Uuid) -> Vec<ConnectionId> {
	realtime
		.connections_by_owner
		.read()
		.await
		.get(&player)
		.map(|connections| connections.iter().copied().collect())
		.unwrap_or_default()
}

async fn send_to_connections(
	realtime: &RealtimeState,
	connection_ids: impl IntoIterator<Item = ConnectionId>,
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	let mut connections = realtime.connections.write().await;
	for connection_id in connection_ids {
		if let Some(connection) = connections.get_mut(&connection_id) {
			let _ = connection.tx.send(make_packet());
		}
	}
}

async fn send_to_players(
	realtime: &RealtimeState,
	players: [Uuid; 2],
	mut make_packet: impl FnMut() -> ClientBoundPacket,
) {
	for player in players {
		send_to_owner_connections(realtime, player, &mut make_packet).await;
	}
}

async fn send_to_player(
	realtime: &RealtimeState,
	player: Uuid,
	make_packet: impl FnMut() -> ClientBoundPacket,
) {
	send_to_owner_connections(realtime, player, make_packet).await;
}

async fn broadcast_to_pair(
	realtime: &RealtimeState,
	players: [Uuid; 2],
	make_packet: impl FnMut() -> ClientBoundPacket,
) {
	let mut participant_ids = HashSet::new();
	for player in players {
		participant_ids.extend(connections_of(realtime, player).await);
	}
	let mut watcher_ids = HashSet::new();
	{
		let watchers = realtime.watchers.read().await;
		for player in players {
			if let Some(player_watchers) = watchers.get(&player) {
				watcher_ids.extend(player_watchers.iter().copied());
			}
		}
	}
	let participant_ids = {
		let connections = realtime.connections.read().await;
		participant_ids
			.into_iter()
			.filter(|connection_id| {
				connections
					.get(connection_id)
					.is_some_and(|connection| connection.acts_as_owner())
			})
			.collect::<Vec<_>>()
	};
	watcher_ids.extend(participant_ids);
	send_to_connections(realtime, watcher_ids, make_packet).await;
}

async fn has_game_connection(realtime: &RealtimeState, player: Uuid) -> bool {
	let connection_ids = connections_of(realtime, player).await;
	let connections = realtime.connections.read().await;
	connection_ids.iter().any(|connection_id| {
		connections
			.get(connection_id)
			.is_some_and(|connection| {
				connection.kind == ClientKind::Game && connection.acts_as_owner()
			})
	})
}

async fn resolve(
	realtime: &RealtimeState,
	request: &OpenRequest,
	result: SharedEmoteResult,
) {
	let request_id = request.id;
	send_to_players(realtime, request.players(), || {
		ClientBoundPacket::SharedEmoteResolved { request_id, result }
	})
	.await;
}

pub(super) async fn request(
	realtime: &RealtimeState,
	gate: &impl SharedEmoteGate,
	from: Uuid,
	target: Uuid,
	emote_id: i32,
	clock: impl Clock,
) -> Result<Option<(Uuid, i64)>, WebsocketError> {
	request_with(realtime, gate, from, target, emote_id, clock, ready(())).await
}

async fn request_with(
	realtime: &RealtimeState,
	gate: &impl SharedEmoteGate,
	from: Uuid,
	target: Uuid,
	emote_id: i32,
	clock: impl Clock,
	before_notifying: impl Future<Output = ()>,
) -> Result<Option<(Uuid, i64)>, WebsocketError> {
	if from == target {
		return Err(WebsocketError::InvalidSharedEmote(
			"You can't share an emote with yourself",
		));
	}
	if is_fake_player(&target) {
		return Err(WebsocketError::InvalidSharedEmote(
			"That player can't share emotes",
		));
	}
	let emote = gate
		.ensure_owns(emote_id)
		.await?
		.ok_or(WebsocketError::InvalidSharedEmote(
			"That emote can't be shared right now",
		))?;
	if !has_game_connection(realtime, target).await {
		return Err(WebsocketError::SharedEmotePartnerOffline);
	}
	let admission = gate
		.admission(target)
		.await?
		.ok_or(WebsocketError::SharedEmotePartnerOffline)?;

	let mut requests = realtime.shared_emotes.write().await;
	let request = {
		let now = clock.now();
		if requests
			.open
			.values()
			.any(|open| open.pairs(from, target) && open.expires_at > now)
		{
			return Err(WebsocketError::SharedEmotePending);
		}
		let generation = realtime.next_emote_generation();
		if admission == Admission::Allowed && !has_game_connection(realtime, target).await {
			return Err(WebsocketError::SharedEmotePartnerOffline);
		}
		let request = OpenRequest {
			id: Uuid::now_v7(),
			emote_id,
			emote,
			from,
			target,
			expires_at: now + REQUEST_LIFETIME_MS,
			accepted: false,
			generation,
		};
		if admission == Admission::Allowed {
			requests.open.insert(request.id, request.clone());
		}
		request
	};
	before_notifying.await;

	send_to_player(realtime, from, || {
		ClientBoundPacket::SharedEmoteRequestSent {
			request_id: request.id,
			target,
			emote_id,
			expires_at: request.expires_at,
			emote: Some(request.emote.clone()),
		}
	})
	.await;

	if admission == Admission::Declined {
		send_to_player(realtime, from, || ClientBoundPacket::SharedEmoteResolved {
			request_id: request.id,
			result: SharedEmoteResult::Declined,
		})
		.await;
		return Ok(None);
	}

	send_to_player(realtime, target, || {
		ClientBoundPacket::SharedEmoteRequested {
			request_id: request.id,
			from,
			emote_id,
			expires_at: request.expires_at,
			emote: Some(request.emote.clone()),
		}
	})
	.await;

	Ok(Some((request.id, request.expires_at)))
}

pub(super) async fn respond(
	realtime: &RealtimeState,
	responder: Uuid,
	request_id: Uuid,
	accept: bool,
	now: i64,
) -> Result<Option<(Uuid, i64)>, WebsocketError> {
	respond_with(realtime, responder, request_id, accept, now, ready(())).await
}

async fn respond_with(
	realtime: &RealtimeState,
	responder: Uuid,
	request_id: Uuid,
	accept: bool,
	now: i64,
	before_notifying: impl Future<Output = ()>,
) -> Result<Option<(Uuid, i64)>, WebsocketError> {
	let mut requests = realtime.shared_emotes.write().await;
	let open = requests
		.open
		.get_mut(&request_id)
		.filter(|open| open.target == responder && !open.accepted)
		.ok_or(WebsocketError::SharedEmoteNotFound)?;
	let result = if open.expires_at <= now {
		SharedEmoteResult::Expired
	} else if accept {
		SharedEmoteResult::Accepted
	} else {
		SharedEmoteResult::Declined
	};
	let request = if result == SharedEmoteResult::Accepted {
		open.accepted = true;
		open.expires_at = now + ACCEPTED_LIFETIME_MS;
		open.clone()
	} else {
		let request = open.clone();
		requests.open.remove(&request_id);
		request
	};
	before_notifying.await;

	resolve(realtime, &request, result).await;
	Ok((result == SharedEmoteResult::Accepted)
		.then_some((request.id, request.expires_at)))
}

pub(super) async fn expire(realtime: &RealtimeState, request_id: Uuid, now: i64) -> bool {
	let expired = {
		let mut requests = realtime.shared_emotes.write().await;
		match requests.open.get(&request_id) {
			Some(open) if open.expires_at <= now => requests.open.remove(&request_id),
			_ => None,
		}
	};
	let Some(request) = expired else {
		return false;
	};
	resolve(realtime, &request, SharedEmoteResult::Expired).await;
	true
}

pub(super) async fn cancel(
	realtime: &RealtimeState,
	player: Uuid,
	request_id: Uuid,
	now: i64,
) -> bool {
	let cancelled = {
		let mut requests = realtime.shared_emotes.write().await;
		match requests.open.get(&request_id) {
			Some(open) if open.involves(player) && open.expires_at > now => {
				requests.open.remove(&request_id)
			}
			_ => None,
		}
	};
	let Some(request) = cancelled else {
		return false;
	};
	resolve(realtime, &request, SharedEmoteResult::Cancelled).await;
	true
}

pub(super) fn expire_later(realtime: &RealtimeState, request_id: Uuid, at: i64) {
	let realtime = realtime.clone();
	tokio::spawn(async move {
		let wait = u64::try_from(at - Utc::now().timestamp_millis()).unwrap_or(0);
		tokio::time::sleep(Duration::from_millis(wait)).await;
		expire(&realtime, request_id, Utc::now().timestamp_millis().max(at)).await;
	});
}

fn pinned_revision(pinned: EmoteInfo, current: Option<EmoteInfo>) -> Option<EmoteInfo> {
	let current = current?;
	(pinned.url == current.url && pinned.cached_info == current.cached_info).then_some(pinned)
}

fn checked_anchor(anchor: EmoteAnchor) -> Result<EmoteAnchor, WebsocketError> {
	anchor.checked().ok_or(WebsocketError::InvalidSharedEmote(
		"The anchor is outside the world",
	))
}

pub(super) fn release(
	runtime: &mut HashMap<Uuid, PlayerRuntimeState>,
	shared: &SharedEmote,
) {
	for player in shared.players {
		if let Some(state) = runtime.get_mut(&player)
			&& state
				.active_emote
				.is_some_and(|active| active.shared.as_ref() == Some(shared))
		{
			state.active_emote = None;
		}
	}
}

pub(super) async fn announce_stop(realtime: &RealtimeState, shared: &SharedEmote) {
	let players = shared.players;
	broadcast_to_pair(realtime, players, || ClientBoundPacket::SharedEmoteStop {
		players,
	})
	.await;
	for player in players {
		broadcast_to_watchers(realtime, player, || {
			ClientBoundPacket::PlayerEmoteStopped { player }
		})
		.await;
	}
}

pub(super) async fn start(
	realtime: &RealtimeState,
	gate: &impl SharedEmoteGate,
	initiator: Uuid,
	request_id: Uuid,
	anchor: EmoteAnchor,
	clock: impl Clock,
) -> Result<(), WebsocketError> {
	start_with(
		realtime,
		gate,
		initiator,
		request_id,
		anchor,
		clock,
		ready(()),
	)
	.await
}

async fn start_with(
	realtime: &RealtimeState,
	gate: &impl SharedEmoteGate,
	initiator: Uuid,
	request_id: Uuid,
	anchor: EmoteAnchor,
	clock: impl Clock,
	before_announcing: impl Future<Output = ()>,
) -> Result<(), WebsocketError> {
	let anchor = checked_anchor(anchor)?;
	let is_ready = |open: &&OpenRequest| open.from == initiator && open.accepted;
	let emote_id = realtime
		.shared_emotes
		.read()
		.await
		.open
		.get(&request_id)
		.filter(is_ready)
		.map(|open| open.emote_id)
		.ok_or(WebsocketError::SharedEmoteNotFound)?;
	let current = gate.ensure_owns(emote_id).await?;

	let _transition = realtime.emote_transitions.lock().await;
	let request = {
		let mut requests = realtime.shared_emotes.write().await;
		if requests.open.get(&request_id).filter(is_ready).is_none() {
			return Err(WebsocketError::SharedEmoteNotFound);
		}
		requests.open.remove(&request_id)
	}
	.ok_or(WebsocketError::SharedEmoteNotFound)?;
	let now = clock.now();

	if request.expires_at <= now {
		resolve(realtime, &request, SharedEmoteResult::Expired).await;
		return Ok(());
	}

	let Some(emote) = pinned_revision(request.emote.clone(), current).map(Some) else {
		resolve(realtime, &request, SharedEmoteResult::Cancelled).await;
		return Ok(());
	};

	let generation = realtime.next_emote_generation();
	for player in request.players() {
		if !has_game_connection(realtime, player).await {
			resolve(realtime, &request, SharedEmoteResult::Cancelled).await;
			return Ok(());
		}
	}

	let shared = SharedEmote {
		emote_id: request.emote_id,
		players: request.players(),
		anchor,
		start_time: now,
	};
	let replaced = {
		let mut runtime = realtime.player_runtime.write().await;
		let mut replaced: Vec<SharedEmote> = Vec::new();
		for player in shared.players {
			let previous = runtime
				.get(&player)
				.and_then(|state| state.active_emote)
				.and_then(|active| active.shared);
			if let Some(previous) = previous
				&& !replaced.contains(&previous)
			{
				replaced.push(previous);
			}
		}
		for previous in &replaced {
			release(&mut runtime, previous);
		}
		for player in shared.players {
			let player_state = runtime.entry(player).or_default();
			player_state.active_emote_definition = emote.clone();
			player_state.active_emote = Some(ActiveEmote {
				emote_id: shared.emote_id,
				started_at: now,
				anchor: None,
				shared: Some(shared),
				generation,
			});
		}
		replaced
	};
	before_announcing.await;

	for previous in &replaced {
		announce_stop(realtime, previous).await;
	}
	for player in shared.players {
		broadcast_to_watchers(realtime, player, || {
			ClientBoundPacket::PlayerEmoteStarted {
				player,
				emote_id: shared.emote_id,
				start_time: Some(now),
				anchor: None,
				request_id: None,
				emote: emote.clone(),
			}
		})
		.await;
	}
	broadcast_to_pair(realtime, shared.players, || {
		ClientBoundPacket::SharedEmoteStart {
			shared,
			emote: emote.clone(),
		}
	})
	.await;

	Ok(())
}

pub(super) async fn cancel_requests_of(realtime: &RealtimeState, player: Uuid, before: u64) {
	let cancelled = {
		let mut requests = realtime.shared_emotes.write().await;
		let ids = requests
			.open
			.values()
			.filter(|open| open.involves(player) && open.generation < before)
			.map(|open| open.id)
			.collect::<Vec<_>>();
		ids.into_iter()
			.filter_map(|id| requests.open.remove(&id))
			.collect::<Vec<_>>()
	};
	for request in cancelled {
		resolve(realtime, &request, SharedEmoteResult::Cancelled).await;
	}
}

pub(super) async fn stop(realtime: &RealtimeState, player: Uuid) {
	let _transition = realtime.emote_transitions.lock().await;
	let ended = {
		let mut runtime = realtime.player_runtime.write().await;
		let shared = runtime
			.get(&player)
			.and_then(|state| state.active_emote)
			.and_then(|active| active.shared);
		if let Some(shared) = &shared {
			release(&mut runtime, shared);
		}
		shared
	};
	if let Some(shared) = ended {
		announce_stop(realtime, &shared).await;
	}
	cancel_requests_of(realtime, player, u64::MAX).await;
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{HashMap, HashSet},
		future::{Future, ready},
		sync::atomic::{AtomicI64, Ordering},
	};

	use entities::sea_orm_active_enums::SharedEmotePrivacy;
	use tokio::{
		sync::{
			mpsc::{self, UnboundedReceiver},
			oneshot,
		},
		task::yield_now,
	};
	use uuid::Uuid;

	use super::{
		ACCEPTED_LIFETIME_MS, Admission, REQUEST_LIFETIME_MS, SharedEmoteGate, admit,
		cancel, expire, request, request_with, respond, respond_with, start, start_with, stop,
	};
	use crate::api::{
		state::{ConnectionId, PlayerRuntimeState, RealtimeConnection, RealtimeState},
		v0::{
			account::{ClientKind, TokenAccess},
			cosmetics::{CachedAssetInfo, EmoteInfo},
			websocket::{
				endpoint::{
					Clock, attach_connection, detach_connection, end_emote_on_disconnect,
					read_runtime, remember_emote_definition, start_emote, stop_emote,
					watch_players,
				},
				structs::{
					ClientBoundPacket, EmoteAnchor, SharedEmote, SharedEmoteResult,
					WebsocketError,
				},
			},
		},
	};

	const NOW: i64 = 1_700_000_000_000;
	const EMOTE: i32 = 42;
	const ANCHOR: EmoteAnchor = EmoteAnchor { x: 100.5, y: 64.0, z: -20.25, yaw: 90.0 };

	struct Gate {
		owns: bool,
		privacy: SharedEmotePrivacy,
		definition: Option<EmoteInfo>,
	}

	impl Gate {
		fn open() -> Self {
			Self::describing(emote("abc123"))
		}

		fn describing(definition: EmoteInfo) -> Self {
			Self { owns: true, privacy: SharedEmotePrivacy::Everyone, definition: Some(definition) }
		}
	}

	impl SharedEmoteGate for Gate {
		async fn ensure_owns(&self, emote_id: i32) -> Result<Option<EmoteInfo>, WebsocketError> {
			self.owns.then(|| self.definition.clone()).ok_or(WebsocketError::UnownedEmote(emote_id))
		}

		async fn admission(&self, _: Uuid) -> Result<Option<Admission>, WebsocketError> {
			Ok(Some(admit(self.privacy, false, false)))
		}
	}

	fn emote(hash: &str) -> EmoteInfo {
		let (name, url) = ("Retired".to_owned(), Some(format!("https://objects.example/emotes/{hash}.zip")));
		EmoteInfo { id: EMOTE, name, url, cached_info: CachedAssetInfo { hash: hash.to_owned() } }
	}

	struct Session {
		player: Uuid,
		rx: UnboundedReceiver<ClientBoundPacket>,
	}

	impl Session {
		fn drain(&mut self) -> Vec<ClientBoundPacket> {
			std::iter::from_fn(|| self.rx.try_recv().ok()).collect()
		}
	}

	fn connection(owner: Uuid, access: TokenAccess) -> (RealtimeConnection, Session) {
		let (tx, rx) = mpsc::unbounded_channel();
		let subscriptions = HashSet::new();
		let kind = ClientKind::Game;
		let connection = RealtimeConnection { owner, tx, subscriptions, kind, access };
		(connection, Session { player: owner, rx })
	}

	async fn connect_as(rt: &RealtimeState, owner: Uuid, access: TokenAccess) -> (Uuid, Session) {
		let (connection, session) = connection(owner, access);
		let id = Uuid::new_v4();
		rt.connections.write().await.insert(id, connection);
		rt.connections_by_owner.write().await.entry(owner).or_default().insert(id);
		(id, session)
	}

	async fn session(rt: &RealtimeState) -> Session {
		connect_as(rt, Uuid::new_v4(), TokenAccess::Player).await.1
	}

	async fn watcher(rt: &RealtimeState, players: Vec<Uuid>) -> (Session, Uuid) {
		let (id, session) = connect_as(rt, Uuid::new_v4(), TokenAccess::Player).await;
		watch_players(rt, id, players).await;
		(session, id)
	}

	async fn disconnect(rt: &RealtimeState, player: Uuid) -> u64 {
		let ids = rt.connections_by_owner.write().await.remove(&player).unwrap_or_default();
		let mut connections = rt.connections.write().await;
		for id in ids {
			connections.remove(&id);
		}
		rt.next_emote_generation()
	}

	async fn close_socket(rt: &RealtimeState, player: Uuid) -> (ConnectionId, u64) {
		let id = *rt.connections_by_owner.read().await[&player].iter().next().unwrap();
		rt.connections.write().await.remove(&id);
		(id, rt.next_emote_generation())
	}

	async fn shared_of(rt: &RealtimeState, player: Uuid) -> Option<SharedEmote> {
		let runtime = rt.player_runtime.read().await;
		runtime.get(&player)?.active_emote?.shared
	}

	async fn definition_of(rt: &RealtimeState, player: Uuid) -> Option<EmoteInfo> {
		rt.player_runtime.read().await.get(&player)?.active_emote_definition.clone()
	}

	async fn open_request(rt: &RealtimeState, a: Uuid, b: Uuid) -> (Uuid, i64) {
		request(rt, &Gate::open(), a, b, EMOTE, NOW).await.unwrap().unwrap()
	}

	async fn accepted_pair(rt: &RealtimeState) -> (Session, Session, Uuid) {
		let (mut a, mut b) = (session(rt).await, session(rt).await);
		let (id, _) = open_request(rt, a.player, b.player).await;
		respond(rt, b.player, id, true, NOW + 1_000).await.unwrap();
		a.drain();
		b.drain();
		(a, b, id)
	}

	async fn dancing_pair(rt: &RealtimeState) -> (Session, Session, SharedEmote) {
		let (mut a, mut b, id) = accepted_pair(rt).await;
		start(rt, &Gate::open(), a.player, id, ANCHOR, NOW + 5_000).await.unwrap();
		a.drain();
		b.drain();
		let shared = shared_of(rt, a.player).await.unwrap();
		(a, b, shared)
	}

	macro_rules! pick {
		($packets:expr, $pattern:pat $(if $guard:expr)? => $value:expr) => {
			$packets.iter().filter_map(|packet| match packet {
				$pattern $(if $guard)? => Some($value),
				_ => None,
			}).collect()
		};
	}

	fn resolved(packets: &[ClientBoundPacket], id: Uuid) -> Vec<SharedEmoteResult> {
		pick!(packets, ClientBoundPacket::SharedEmoteResolved { request_id, result } if *request_id == id => *result)
	}

	fn shared_starts(packets: &[ClientBoundPacket]) -> Vec<SharedEmote> {
		pick!(packets, ClientBoundPacket::SharedEmoteStart { shared, .. } => *shared)
	}

	fn shared_stops(packets: &[ClientBoundPacket]) -> Vec<[Uuid; 2]> {
		pick!(packets, ClientBoundPacket::SharedEmoteStop { players } => *players)
	}

	fn solo_stops(packets: &[ClientBoundPacket]) -> Vec<Uuid> {
		pick!(packets, ClientBoundPacket::PlayerEmoteStopped { player } => *player)
	}

	fn described(packets: &[ClientBoundPacket]) -> Vec<Option<EmoteInfo>> {
		pick!(packets, ClientBoundPacket::SharedEmoteStart { emote, .. } | ClientBoundPacket::PlayerEmoteStarted { emote, .. } => emote.clone())
	}

	struct Pause {
		reached: oneshot::Receiver<()>,
		resume: oneshot::Sender<()>,
	}

	impl Pause {
		fn new() -> (Self, impl Future<Output = ()> + Send + 'static) {
			let (reached_tx, reached) = oneshot::channel();
			let (resume, resumed) = oneshot::channel::<()>();
			let hook = async move {
				let _ = reached_tx.send(());
				let _ = resumed.await;
			};
			(Self { reached, resume }, hook)
		}

		async fn reached(&mut self) {
			while self.reached.try_recv().is_err() {
				yield_now().await;
			}
		}

		async fn release(self) {
			settle().await;
			let _ = self.resume.send(());
		}
	}

	async fn settle() {
		for _ in 0..64 {
			yield_now().await;
		}
	}

	#[test]
	fn privacy_and_blocks_decide_admission() {
		use SharedEmotePrivacy::*;
		assert_eq!(admit(Everyone, false, false), Admission::Allowed);
		assert_eq!(admit(Everyone, true, true), Admission::Declined);
		assert_eq!(admit(Friends, false, true), Admission::Allowed);
		assert_eq!(admit(Friends, false, false), Admission::Declined);
		assert_eq!(admit(Friends, true, true), Admission::Declined);
		assert_eq!(admit(Nobody, false, true), Admission::Declined);
	}

	#[tokio::test]
	async fn request_accept_start_reaches_both_with_the_pinned_revision() {
		let rt = RealtimeState::default();
		remember_emote_definition(&rt, emote("other")).await;
		let pinned = Some(emote("abc123"));
		let (mut a, mut b) = (session(&rt).await, session(&rt).await);
		let (mut both, _) = watcher(&rt, vec![a.player, b.player]).await;
		let (mut only_b, _) = watcher(&rt, vec![b.player]).await;

		let (id, expires_at) = open_request(&rt, a.player, b.player).await;
		assert_eq!(expires_at, NOW + REQUEST_LIFETIME_MS);
		assert!(matches!(a.drain().as_slice(), [ClientBoundPacket::SharedEmoteRequestSent {
			request_id, target, emote_id: EMOTE, expires_at: at, emote,
		}] if *request_id == id && *target == b.player && *at == expires_at && *emote == pinned));
		assert!(matches!(b.drain().as_slice(), [ClientBoundPacket::SharedEmoteRequested {
			request_id, from, emote_id: EMOTE, expires_at: at, emote,
		}] if *request_id == id && *from == a.player && *at == expires_at && *emote == pinned));

		let accepted = respond(&rt, b.player, id, true, NOW + 1_000).await.unwrap();
		assert_eq!(accepted, Some((id, NOW + 1_000 + ACCEPTED_LIFETIME_MS)));
		assert_eq!(resolved(&a.drain(), id), vec![SharedEmoteResult::Accepted]);
		assert_eq!(resolved(&b.drain(), id), vec![SharedEmoteResult::Accepted]);
		assert!(both.drain().is_empty());

		let at = NOW + 1_000 + ACCEPTED_LIFETIME_MS - 1;
		let renamed = EmoteInfo { name: "Renamed".to_owned(), ..emote("abc123") };
		start(&rt, &Gate::describing(renamed), a.player, id, ANCHOR, at).await.unwrap();

		let players = [a.player, b.player];
		let expected = SharedEmote { emote_id: EMOTE, players, anchor: ANCHOR, start_time: at };
		for packets in [a.drain(), b.drain()] {
			assert_eq!(shared_starts(&packets), vec![expected]);
			assert!(described(&packets).iter().all(|emote| *emote == pinned));
		}
		let watched = both.drain();
		assert_eq!(shared_starts(&watched), vec![expected]);
		assert!(matches!(watched.last(), Some(ClientBoundPacket::SharedEmoteStart { .. })));
		assert_eq!(described(&watched), vec![pinned.clone(); 3]);
		let solo_starts = watched.iter().filter_map(|packet| match packet {
			ClientBoundPacket::PlayerEmoteStarted {
				player, emote_id: EMOTE, start_time: Some(time), anchor: None, request_id: None, ..
			} if *time == at => Some(*player),
			_ => None,
		});
		assert_eq!(solo_starts.collect::<Vec<_>>(), players);
		assert_eq!(shared_starts(&only_b.drain()), vec![expected]);
		for player in players {
			assert_eq!(shared_of(&rt, player).await, Some(expected));
			assert_eq!(definition_of(&rt, player).await, pinned);
		}

		let (_late, late) = watcher(&rt, Vec::new()).await;
		let (newly, _) = watch_players(&rt, late, players.to_vec()).await.unwrap();
		let snapshot = read_runtime(&*rt.player_runtime.read().await, &newly);
		let both = |value| HashMap::from(players.map(|player| (player, value)));
		assert_eq!(snapshot.active_emotes, both(EMOTE));
		assert_eq!(snapshot.active_emote_starts, HashMap::from(players.map(|player| (player, at))));
		assert_eq!(snapshot.shared_emotes, HashMap::from(players.map(|player| (player, expected))));
		assert!(snapshot.active_emote_anchors.is_empty());
	}

	#[derive(Clone, Copy, Debug)]
	enum End {
		Decline,
		AnswerLate,
		Expire,
		Cancel(usize),
		Stop(usize),
		StartLate,
		Replaced,
		Unreadable,
	}

	#[tokio::test]
	async fn every_way_a_request_ends_tells_both_and_frees_the_pair() {
		use End::*;
		use SharedEmoteResult::*;
		for (accepted, end, result) in [
			(false, Decline, Declined),
			(false, AnswerLate, Expired),
			(false, Expire, Expired),
			(false, Cancel(0), Cancelled),
			(false, Cancel(1), Cancelled),
			(true, Cancel(0), Cancelled),
			(true, Cancel(1), Cancelled),
			(true, Stop(1), Cancelled),
			(true, Expire, Expired),
			(true, StartLate, Expired),
			(true, Replaced, Cancelled),
			(true, Unreadable, Cancelled),
		] {
			let rt = RealtimeState::default();
			let (mut a, mut b) = (session(&rt).await, session(&rt).await);
			let (mut both, _) = watcher(&rt, vec![a.player, b.player]).await;
			let players = [a.player, b.player];
			let (id, mut deadline) = open_request(&rt, a.player, b.player).await;
			if accepted {
				respond(&rt, b.player, id, true, NOW + 1_000).await.unwrap();
				deadline = NOW + 1_000 + ACCEPTED_LIFETIME_MS;
			}
			a.drain();
			b.drain();
			let unreadable = Gate { definition: None, ..Gate::open() };
			let gate = match end {
				Replaced => Gate::describing(emote("def456")),
				Unreadable => unreadable,
				_ => Gate::open(),
			};
			match end {
				Decline => assert_eq!(respond(&rt, b.player, id, false, NOW + 1).await.unwrap(), None),
				AnswerLate => assert_eq!(respond(&rt, b.player, id, true, deadline).await.unwrap(), None),
				Expire => {
					assert!(!expire(&rt, id, deadline - 1).await);
					assert!(expire(&rt, id, deadline).await);
				}
				Cancel(who) => assert!(cancel(&rt, players[who], id, NOW + 2_000).await),
				Stop(who) => stop(&rt, players[who]).await,
				StartLate => start(&rt, &gate, a.player, id, ANCHOR, deadline).await.unwrap(),
				Replaced | Unreadable => {
					start(&rt, &gate, a.player, id, ANCHOR, NOW + 5_000).await.unwrap()
				}
			}

			for packets in [a.drain(), b.drain()] {
				assert_eq!(resolved(&packets, id), vec![result], "{end:?}");
				assert!(shared_starts(&packets).is_empty(), "{end:?}");
			}
			assert!(both.drain().is_empty(), "{end:?}");
			for player in players {
				assert_eq!(shared_of(&rt, player).await, None, "{end:?}");
				assert_eq!(definition_of(&rt, player).await, None, "{end:?}");
			}
			assert!(rt.shared_emotes.read().await.open.is_empty(), "{end:?}");
			assert!(matches!(
				respond(&rt, b.player, id, true, NOW + 3_000).await,
				Err(WebsocketError::SharedEmoteNotFound)
			));
			assert!(matches!(
				start(&rt, &Gate::open(), a.player, id, ANCHOR, NOW + 3_000).await,
				Err(WebsocketError::SharedEmoteNotFound)
			));
			let again = request(&rt, &Gate::open(), a.player, b.player, EMOTE, NOW + 3_000).await;
			assert!(again.is_ok(), "{end:?}");
		}
	}

	#[tokio::test]
	async fn invalid_requests_and_starts_are_refused_without_side_effects() {
		use WebsocketError::*;
		let rt = RealtimeState::default();
		let (mut a, mut b) = (session(&rt).await, session(&rt).await);
		let staff_only = Uuid::new_v4();
		let (_, mut staff) = connect_as(&rt, staff_only, TokenAccess::SpecialChat).await;
		let unowned = Gate { owns: false, ..Gate::open() };
		let undescribed = Gate { definition: None, ..Gate::open() };
		let open = Gate::open();
		let (from, to) = (a.player, b.player);

		let refused = [
			request(&rt, &unowned, from, to, EMOTE, NOW).await,
			request(&rt, &undescribed, from, to, EMOTE, NOW).await,
			request(&rt, &open, from, from, EMOTE, NOW).await,
			request(&rt, &open, from, Uuid::new_v4(), EMOTE, NOW).await,
			request(&rt, &open, from, staff_only, EMOTE, NOW).await,
		];
		assert!(matches!(refused, [
			Err(UnownedEmote(EMOTE)),
			Err(InvalidSharedEmote(_)),
			Err(InvalidSharedEmote(_)),
			Err(SharedEmotePartnerOffline),
			Err(SharedEmotePartnerOffline),
		]));
		assert!(a.drain().is_empty() && b.drain().is_empty() && staff.drain().is_empty());
		assert!(rt.shared_emotes.read().await.open.is_empty());
		let friends_only = Gate { privacy: SharedEmotePrivacy::Friends, ..Gate::open() };
		assert_eq!(request(&rt, &friends_only, from, to, EMOTE, NOW).await.unwrap(), None);
		let to_a = a.drain();
		let Some(ClientBoundPacket::SharedEmoteRequestSent { request_id, .. }) = to_a.first() else {
			panic!("A should hear its request was sent: {to_a:?}");
		};
		assert_eq!(resolved(&to_a, *request_id), vec![SharedEmoteResult::Declined]);
		assert!(b.drain().is_empty() && rt.shared_emotes.read().await.open.is_empty());

		let (id, _) = open_request(&rt, from, to).await;
		assert!(matches!(request(&rt, &open, from, to, EMOTE, NOW).await, Err(SharedEmotePending)));
		assert!(matches!(request(&rt, &open, to, from, EMOTE, NOW).await, Err(SharedEmotePending)));
		assert!(matches!(respond(&rt, from, id, true, NOW).await, Err(SharedEmoteNotFound)));
		assert!(matches!(start(&rt, &open, from, id, ANCHOR, NOW).await, Err(SharedEmoteNotFound)));
		respond(&rt, to, id, true, NOW + 1).await.unwrap();
		let far = EmoteAnchor { x: 1.0e9, ..ANCHOR };
		let refused = [
			start(&rt, &open, to, id, ANCHOR, NOW + 2).await,
			start(&rt, &unowned, from, id, ANCHOR, NOW + 2).await,
			start(&rt, &open, from, id, far, NOW + 2).await,
		];
		assert!(matches!(refused, [
			Err(SharedEmoteNotFound),
			Err(UnownedEmote(EMOTE)),
			Err(InvalidSharedEmote(_)),
		]));
		assert_eq!(shared_of(&rt, from).await, None);
		let turned = EmoteAnchor { yaw: 270.0, ..ANCHOR };
		start(&rt, &open, from, id, turned, NOW + 3).await.unwrap();
		assert!((shared_of(&rt, from).await.unwrap().anchor.yaw + 90.0).abs() < 1.0e-4);
	}

	#[tokio::test]
	async fn cancels_that_do_not_apply_change_nothing() {
		let rt = RealtimeState::default();
		let (mut a, mut b, shared) = dancing_pair(&rt).await;
		let mut c = session(&rt).await;
		assert!(!cancel(&rt, a.player, Uuid::now_v7(), NOW).await);

		let (id, expires_at) = open_request(&rt, a.player, c.player).await;
		a.drain();
		c.drain();
		assert!(!cancel(&rt, b.player, id, NOW + 1_000).await);
		assert!(!cancel(&rt, a.player, id, expires_at).await);
		assert!(a.drain().is_empty() && b.drain().is_empty() && c.drain().is_empty());
		assert!(expire(&rt, id, expires_at).await);
		assert!(!cancel(&rt, a.player, id, NOW + 2_000).await);

		let (id, _) = open_request(&rt, a.player, c.player).await;
		a.drain();
		assert!(cancel(&rt, c.player, id, NOW + 1_000).await);
		assert_eq!(shared_of(&rt, a.player).await, Some(shared));
		assert!(shared_stops(&a.drain()).is_empty());
		assert!(b.drain().is_empty());
	}

	#[tokio::test]
	async fn every_way_out_of_a_duo_releases_both() {
		for end in 0..6 {
			let rt = RealtimeState::default();
			let (mut a, mut b, shared) = dancing_pair(&rt).await;
			let (mut both, _) = watcher(&rt, vec![a.player, b.player]).await;
			let mut stays = None;
			match end {
				0 => stop(&rt, b.player).await,
				1 => stop_emote(&rt, a.player, Some(EMOTE), None).await,
				2 => {
					stop_emote(&rt, b.player, Some(EMOTE), Some(shared.start_time - 1)).await;
					assert_eq!(shared_of(&rt, a.player).await, Some(shared));
					stop_emote(&rt, b.player, Some(EMOTE), Some(shared.start_time)).await;
				}
				3 => {
					start_emote(&rt, a.player, 7, None, NOW + 9_000, None).await;
					stays = Some(7);
				}
				4 => {
					let c = session(&rt).await;
					let (id, _) = open_request(&rt, a.player, c.player).await;
					respond(&rt, c.player, id, true, NOW + 1).await.unwrap();
					start(&rt, &Gate::open(), a.player, id, ANCHOR, NOW + 2).await.unwrap();
					let with_c = shared_of(&rt, c.player).await.map(|shared| shared.players);
					assert_eq!(with_c, Some([a.player, c.player]));
					stays = Some(EMOTE);
				}
				_ => {
					let departed = disconnect(&rt, a.player).await;
					end_emote_on_disconnect(&rt, a.player, departed).await;
				}
			}

			let a_emote = rt.player_runtime.read().await.get(&a.player).and_then(|a| a.active_emote);
			assert_eq!(a_emote.map(|active| active.emote_id), stays, "{end}");
			assert_eq!(shared_of(&rt, b.player).await, None, "{end}");
			assert_eq!(shared_stops(&b.drain()), vec![shared.players], "{end}");
			let watched = both.drain();
			assert_eq!(shared_stops(&watched), vec![shared.players], "{end}");
			if end == 0 {
				assert_eq!(solo_stops(&watched), shared.players);
			}
			a.drain();
			stop(&rt, b.player).await;
			assert!(shared_stops(&a.drain()).is_empty(), "{end}");
		}
	}

	#[tokio::test]
	async fn stopping_a_duo_also_cancels_the_stoppers_open_requests() {
		let rt = RealtimeState::default();
		let (mut a, mut b, shared) = dancing_pair(&rt).await;
		let (mut c, mut d) = (session(&rt).await, session(&rt).await);
		let (to_c, _) = open_request(&rt, a.player, c.player).await;
		respond(&rt, c.player, to_c, true, NOW + 6_000).await.unwrap();
		let (from_d, _) = open_request(&rt, d.player, a.player).await;
		for session in [&mut a, &mut b, &mut c, &mut d] {
			session.drain();
		}

		stop(&rt, a.player).await;

		let to_a = a.drain();
		assert_eq!(shared_stops(&to_a), vec![shared.players]);
		assert_eq!(shared_stops(&b.drain()), vec![shared.players]);
		for (id, other) in [(to_c, c.drain()), (from_d, d.drain())] {
			assert_eq!(resolved(&to_a, id), vec![SharedEmoteResult::Cancelled]);
			assert_eq!(resolved(&other, id), vec![SharedEmoteResult::Cancelled]);
		}
		assert!(rt.shared_emotes.read().await.open.is_empty());
		assert_eq!(shared_of(&rt, c.player).await, None);
	}

	#[tokio::test]
	async fn a_stop_during_an_accept_is_announced_after_the_accept() {
		let rt = RealtimeState::default();
		let (mut a, mut b) = (session(&rt).await, session(&rt).await);
		let (id, _) = open_request(&rt, a.player, b.player).await;
		a.drain();
		b.drain();
		let (mut pause, hook) = Pause::new();
		let accepting = tokio::spawn({
			let (rt, responder) = (rt.clone(), b.player);
			async move { respond_with(&rt, responder, id, true, NOW + 1_000, hook).await.unwrap() }
		});
		pause.reached().await;

		let stopping = tokio::spawn({
			let (rt, player) = (rt.clone(), a.player);
			async move { stop(&rt, player).await }
		});
		pause.release().await;
		accepting.await.unwrap();
		stopping.await.unwrap();

		for packets in [a.drain(), b.drain()] {
			let results = resolved(&packets, id);
			assert_eq!(results, vec![SharedEmoteResult::Accepted, SharedEmoteResult::Cancelled]);
		}
		assert!(rt.shared_emotes.read().await.open.is_empty());
	}

	#[tokio::test]
	async fn a_partner_leaving_as_the_request_opens_is_cancelled_after_the_invite() {
		let rt = RealtimeState::default();
		let (mut a, b) = (session(&rt).await, session(&rt).await);
		let (mut pause, hook) = Pause::new();
		let requesting = tokio::spawn({
			let (rt, from, target) = (rt.clone(), a.player, b.player);
			async move {
				let gate = Gate::open();
				request_with(&rt, &gate, from, target, EMOTE, NOW, hook).await.unwrap().unwrap()
			}
		});
		pause.reached().await;

		let departed = disconnect(&rt, b.player).await;
		let leaving = tokio::spawn({
			let (rt, target) = (rt.clone(), b.player);
			async move { end_emote_on_disconnect(&rt, target, departed).await }
		});
		pause.release().await;
		let (id, _) = requesting.await.unwrap();
		leaving.await.unwrap();

		let packets = a.drain();
		let sent = packets.iter().position(|packet| {
			matches!(packet, ClientBoundPacket::SharedEmoteRequestSent { request_id, .. } if *request_id == id)
		});
		let cancelled = packets.iter().position(|packet| {
			matches!(packet, ClientBoundPacket::SharedEmoteResolved {
				request_id,
				result: SharedEmoteResult::Cancelled,
			} if *request_id == id)
		});
		assert!(sent.unwrap() < cancelled.unwrap());
		assert!(rt.shared_emotes.read().await.open.is_empty());
	}

	fn watched_sessions(packets: Vec<ClientBoundPacket>) -> HashMap<Uuid, SharedEmote> {
		let mut seen: HashMap<Uuid, SharedEmote> = HashMap::new();
		for packet in packets {
			match packet {
				ClientBoundPacket::SharedEmoteStart { shared, .. } => {
					seen.extend(shared.players.map(|player| (player, shared)));
				}
				ClientBoundPacket::SharedEmoteStop { players } => {
					seen.retain(|player, shared| {
						!(players.contains(player) && shared.players == players)
					});
				}
				_ => {}
			}
		}
		seen
	}

	#[tokio::test]
	async fn a_start_waiting_to_announce_holds_back_the_competing_start() {
		let rt = RealtimeState::default();
		let (a, b, c) = (session(&rt).await, session(&rt).await, session(&rt).await);
		let (mut all, _) = watcher(&rt, vec![a.player, b.player, c.player]).await;
		let (with_a, _) = open_request(&rt, a.player, b.player).await;
		let (with_c, _) = open_request(&rt, c.player, b.player).await;
		for id in [with_a, with_c] {
			respond(&rt, b.player, id, true, NOW + 1).await.unwrap();
		}
		all.drain();

		let (mut pause, hook) = Pause::new();
		let first = tokio::spawn({
			let (rt, initiator) = (rt.clone(), a.player);
			async move { start_with(&rt, &Gate::open(), initiator, with_a, ANCHOR, NOW + 2, hook).await }
		});
		pause.reached().await;
		let second = tokio::spawn({
			let (rt, initiator) = (rt.clone(), c.player);
			async move { start(&rt, &Gate::open(), initiator, with_c, ANCHOR, NOW + 3).await }
		});
		pause.release().await;
		for racer in [first, second] {
			racer.await.unwrap().unwrap();
		}

		let seen = watched_sessions(all.drain());
		for player in [a.player, b.player, c.player] {
			assert_eq!(seen.get(&player).copied(), shared_of(&rt, player).await, "{player}");
		}
	}

	#[tokio::test]
	async fn a_disconnect_ends_only_what_the_closed_connection_started() {
		for fresh in [false, true] {
			let rt = RealtimeState::default();
			let (a, mut b, mut c) = (session(&rt).await, session(&rt).await, session(&rt).await);
			start_emote(&rt, a.player, 7, None, NOW, None).await;
			let (stale, _) = open_request(&rt, a.player, b.player).await;
			let departed = disconnect(&rt, a.player).await;
			let _reconnected = connect_as(&rt, a.player, TokenAccess::Player).await;
			let mut kept = None;
			if fresh {
				kept = Some(open_request(&rt, a.player, c.player).await.0);
				start_emote(&rt, a.player, 8, None, NOW + 2, None).await;
			}
			b.drain();
			c.drain();

			end_emote_on_disconnect(&rt, a.player, departed).await;

			let active = rt.player_runtime.read().await.get(&a.player).and_then(|a| a.active_emote);
			assert_eq!(active.map(|active| active.emote_id), fresh.then_some(8));
			assert_eq!(resolved(&b.drain(), stale), vec![SharedEmoteResult::Cancelled]);
			assert!(c.drain().is_empty());
			let open = rt.shared_emotes.read().await.open.keys().copied().collect::<Vec<_>>();
			assert_eq!(open, Vec::from_iter(kept));
		}
	}

	#[tokio::test]
	async fn a_reconnect_during_disconnect_cleanup_still_frees_the_partner() {
		let rt = RealtimeState::default();
		let (a, mut b, shared) = dancing_pair(&rt).await;
		let (mut only_a, _) = watcher(&rt, vec![a.player]).await;
		let (closing, departed) = close_socket(&rt, a.player).await;

		let transitions = rt.emote_transitions.lock().await;
		let detach = tokio::spawn({
			let (rt, owner) = (rt.clone(), a.player);
			async move { detach_connection(&rt, closing, owner, true, departed).await }
		});
		while rt.connections_by_owner.read().await.contains_key(&a.player) {
			yield_now().await;
		}
		let (reconnection, _reconnected) = connection(a.player, TokenAccess::Player);
		let attach = tokio::spawn({
			let rt = rt.clone();
			let load = ready(Ok::<_, ()>(PlayerRuntimeState::default()));
			async move { attach_connection(&rt, Uuid::new_v4(), reconnection, load).await.is_ok() }
		});
		settle().await;
		drop(transitions);

		assert!(!detach.await.unwrap());
		assert!(attach.await.unwrap());
		assert_eq!(shared_of(&rt, a.player).await, None);
		assert_eq!(shared_of(&rt, b.player).await, None);
		assert_eq!(shared_stops(&b.drain()), vec![shared.players]);
		assert_eq!(solo_stops(&only_a.drain()), vec![a.player]);
	}

	#[tokio::test]
	async fn a_start_after_the_partner_left_but_before_its_cleanup_leaves_no_session() {
		let rt = RealtimeState::default();
		let (mut a, b, id) = accepted_pair(&rt).await;
		let (mut both, _) = watcher(&rt, vec![a.player, b.player]).await;
		let (closing, departed) = close_socket(&rt, b.player).await;

		start(&rt, &Gate::open(), a.player, id, ANCHOR, NOW + 2_000).await.unwrap();
		detach_connection(&rt, closing, b.player, true, departed).await;

		assert_eq!(shared_of(&rt, a.player).await, None);
		assert_eq!(shared_of(&rt, b.player).await, None);
		assert_eq!(resolved(&a.drain(), id), vec![SharedEmoteResult::Cancelled]);
		assert!(watched_sessions(both.drain()).is_empty());
	}

	#[tokio::test]
	async fn staff_sockets_get_no_invitations_but_keep_their_subscriptions() {
		let rt = RealtimeState::default();
		let (mut a, mut b) = (session(&rt).await, session(&rt).await);
		let (_, mut a_staff) = connect_as(&rt, a.player, TokenAccess::SpecialChat).await;
		let (watching, mut b_staff) = connect_as(&rt, b.player, TokenAccess::SpecialChat).await;
		watch_players(&rt, watching, vec![a.player]).await;

		let (id, _) = open_request(&rt, a.player, b.player).await;
		respond(&rt, b.player, id, true, NOW + 1_000).await.unwrap();
		start(&rt, &Gate::open(), a.player, id, ANCHOR, NOW + 5_000).await.unwrap();
		stop(&rt, b.player).await;

		assert!(matches!(a.drain()[0], ClientBoundPacket::SharedEmoteRequestSent { .. }));
		assert!(matches!(b.drain()[0], ClientBoundPacket::SharedEmoteRequested { .. }));
		assert!(a_staff.drain().is_empty());
		let watched = b_staff.drain();
		assert!(resolved(&watched, id).is_empty());
		assert_eq!(shared_starts(&watched).len(), 1);
		assert_eq!(shared_stops(&watched).len(), 1);
	}

	impl Clock for &AtomicI64 {
		fn now(&self) -> i64 {
			self.load(Ordering::SeqCst)
		}
	}

	struct SlowGate<'a> {
		clock: &'a AtomicI64,
	}

	impl SharedEmoteGate for SlowGate<'_> {
		async fn ensure_owns(&self, _: i32) -> Result<Option<EmoteInfo>, WebsocketError> {
			self.clock.fetch_add(3_000, Ordering::SeqCst);
			Ok(Some(emote("abc123")))
		}

		async fn admission(&self, _: Uuid) -> Result<Option<Admission>, WebsocketError> {
			Ok(Some(Admission::Allowed))
		}
	}

	#[tokio::test]
	async fn start_times_are_read_after_validation_and_the_transition_lock() {
		let rt = RealtimeState::default();
		let (mut a, _, id) = accepted_pair(&rt).await;
		let clock = AtomicI64::new(NOW + 2_000);
		let slow = SlowGate { clock: &clock };
		let transition = rt.emote_transitions.lock().await;
		let (started, ()) = tokio::join!(
			start(&rt, &slow, a.player, id, ANCHOR, &clock),
			async {
				settle().await;
				assert_eq!(shared_of(&rt, a.player).await, None);
				clock.fetch_add(4_000, Ordering::SeqCst);
				drop(transition);
			}
		);
		started.unwrap();
		let shared = shared_of(&rt, a.player).await.unwrap();
		assert_eq!(shared.start_time, NOW + 9_000);
		assert_eq!(shared_starts(&a.drain()), vec![shared]);

		let player = Uuid::new_v4();
		let (mut viewer, _) = watcher(&rt, vec![player]).await;
		let transition = rt.emote_transitions.lock().await;
		let (started_at, ()) = tokio::join!(start_emote(&rt, player, EMOTE, None, &clock, None), async {
			settle().await;
			clock.fetch_add(4_000, Ordering::SeqCst);
			drop(transition);
		});
		assert_eq!(started_at, NOW + 13_000);
		assert!(viewer.drain().iter().any(|packet| matches!(packet,
			ClientBoundPacket::PlayerEmoteStarted { start_time: Some(time), .. } if *time == started_at
		)));

		let (mut a, mut b) = (session(&rt).await, session(&rt).await);
		let opened = request(&rt, &slow, a.player, b.player, EMOTE, &clock);
		let (_, expires_at) = opened.await.unwrap().unwrap();
		assert_eq!(expires_at, NOW + 16_000 + REQUEST_LIFETIME_MS);
		for packets in [a.drain(), b.drain()] {
			assert!(packets.iter().all(|packet| match packet {
				ClientBoundPacket::SharedEmoteRequestSent { expires_at: at, .. }
				| ClientBoundPacket::SharedEmoteRequested { expires_at: at, .. } => *at == expires_at,
				_ => true,
			}));
		}
	}

	struct LeavingGate<'a> {
		rt: &'a RealtimeState,
		leaving: Uuid,
	}

	impl SharedEmoteGate for LeavingGate<'_> {
		async fn ensure_owns(&self, _: i32) -> Result<Option<EmoteInfo>, WebsocketError> {
			Ok(Some(emote("abc123")))
		}

		async fn admission(&self, _: Uuid) -> Result<Option<Admission>, WebsocketError> {
			let (closing, departed) = close_socket(self.rt, self.leaving).await;
			detach_connection(self.rt, closing, self.leaving, true, departed).await;
			Ok(Some(Admission::Allowed))
		}
	}

	#[tokio::test]
	async fn a_partner_who_leaves_while_the_request_is_checked_gets_no_invite() {
		let rt = RealtimeState::default();
		let (mut a, b) = (session(&rt).await, session(&rt).await);
		let gate = LeavingGate { rt: &rt, leaving: b.player };

		let result = request(&rt, &gate, a.player, b.player, EMOTE, NOW).await;

		assert!(matches!(result, Err(WebsocketError::SharedEmotePartnerOffline)));
		assert!(a.drain().is_empty());
		assert!(rt.shared_emotes.read().await.open.is_empty());
	}
}
