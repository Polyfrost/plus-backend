use std::{
	collections::{HashMap, HashSet},
	sync::{
		Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak,
		atomic::{AtomicU64, Ordering},
	},
};

use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::{BodySlot, PlayerStatus};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::{Mutex, Notify, OwnedMutexGuard, RwLock, mpsc};
use uuid::Uuid;

use crate::api::v0::{
	account::{ClientKind, TokenAccess},
	bonuses::{BonusRef, BonusRetries, DiscoveryWrites},
	cosmetics::EmoteInfo,
	websocket::{
		SharedEmoteRequests,
		colors::SlotColors,
		structs::{ClientBoundPacket, EmoteAnchor, PetPlay, SharedEmote},
	},
};

pub type ConnectionId = Uuid;

/// Everything the websocket layer keeps in memory, shared across connections.
#[derive(Debug, Clone, Default)]
pub struct RealtimeState {
	pub connections: Arc<RwLock<HashMap<ConnectionId, RealtimeConnection>>>,
	pub connections_by_owner: Arc<RwLock<HashMap<Uuid, HashSet<ConnectionId>>>>,
	pub player_runtime: Arc<RwLock<HashMap<Uuid, PlayerRuntimeState>>>,
	pub watchers: Arc<RwLock<HashMap<Uuid, HashSet<ConnectionId>>>>,
	/// Delegates' sockets, keyed by the player they act as. They get that
	/// player's packets but are not their connection: no presence, no session.
	pub delegate_listeners: Arc<
		RwLock<HashMap<Uuid, HashMap<ConnectionId, mpsc::UnboundedSender<ClientBoundPacket>>>>,
	>,
	pub playtime: PlaytimeSessions,
	pub shared_emotes: Arc<RwLock<SharedEmoteRequests>>,
	pub emote_transitions: Arc<Mutex<()>>,
	pub emote_generations: Arc<AtomicU64>,
	pub emote_definitions: Arc<RwLock<HashMap<i32, EmoteInfo>>>,
	pub bonus_updates: PlayerLocks,
	pub discovery_deliveries: PlayerLocks,
	pub discovery_writes: Arc<StdMutex<DiscoveryWrites>>,
	pub bonus_retries: BonusRetries,
	pub socket_retirements: SocketRetirements,
	pub player_updates: PlayerLocks,
	pub slot_rules: Arc<RwLock<()>>,
}

impl RealtimeState {
	pub fn next_emote_generation(&self) -> u64 {
		self.emote_generations.fetch_add(1, Ordering::Relaxed) + 1
	}

	pub async fn connection_counts(&self) -> ConnectionCounts {
		let connections = self.connections.read().await;
		let total = connections.len();
		let game = connections
			.values()
			.filter(|connection| connection.kind == ClientKind::Game)
			.count();

		ConnectionCounts {
			total,
			game,
			other: total - game,
		}
	}
}

#[derive(Debug, Clone, Default)]
pub struct PlayerLocks(Arc<StdMutex<HashMap<Uuid, Weak<Mutex<()>>>>>);

impl PlayerLocks {
	pub async fn lock(&self, player: Uuid) -> OwnedMutexGuard<()> {
		let lock = {
			let mut locks = self.0.lock().unwrap_or_else(PoisonError::into_inner);
			match locks.get(&player).and_then(Weak::upgrade) {
				Some(lock) => lock,
				None => {
					locks.retain(|_, lock| lock.strong_count() > 0);
					let lock = Arc::new(Mutex::new(()));
					locks.insert(player, Arc::downgrade(&lock));
					lock
				}
			}
		};
		lock.lock_owned().await
	}
}

#[derive(Debug, Clone, Default)]
pub struct SocketRetirements(Arc<StdMutex<HashMap<ConnectionId, Arc<Notify>>>>);

impl SocketRetirements {
	fn signals(&self) -> MutexGuard<'_, HashMap<ConnectionId, Arc<Notify>>> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	pub fn watch(&self, connection_id: ConnectionId) -> Arc<Notify> {
		self.signals().entry(connection_id).or_default().clone()
	}

	pub fn forget(&self, connection_id: ConnectionId) {
		self.signals().remove(&connection_id);
	}

	pub fn retire(&self, connection_id: ConnectionId) -> bool {
		let signal = self.signals().get(&connection_id).cloned();
		signal.inspect(|signal| signal.notify_one()).is_some()
	}
}

/// Live websocket connections, split by what kind of client holds them.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ConnectionCounts {
	pub total: usize,
	pub game: usize,
	pub other: usize,
}

/// A single live websocket connection.
#[derive(Debug, Clone)]
pub struct RealtimeConnection {
	pub owner: Uuid,
	pub tx: mpsc::UnboundedSender<ClientBoundPacket>,
	pub subscriptions: HashSet<Uuid>,
	pub kind: ClientKind,
	pub access: TokenAccess,
}

impl RealtimeConnection {
	pub fn acts_as_owner(&self) -> bool {
		self.access != TokenAccess::SpecialChat
	}
}

/// Cosmetic state a player is currently broadcasting to watchers.
#[derive(Debug, Clone, Default)]
pub struct PlayerRuntimeState {
	pub equipped: HashMap<BodySlot, i32>,
	pub active_emote: Option<ActiveEmote>,
	pub active_emote_definition: Option<EmoteInfo>,
	pub particle_color: Option<i32>,
	pub colors: HashMap<i32, SlotColors>,
	pub aura_defaults: HashSet<i32>,
	/// The status the player picked, `None` meaning derive it from their connection.
	pub status_override: Option<PlayerStatus>,
	pub bonuses: Vec<BonusRef>,
	pub pet_play: PetPlay,
	pub live: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveEmote {
	pub emote_id: i32,
	pub started_at: i64,
	pub anchor: Option<EmoteAnchor>,
	pub shared: Option<SharedEmote>,
	pub generation: u64,
}

/// The status other players see: offline while disconnected, otherwise the
/// player's pick, defaulting to online.
pub fn effective_status(
	connected: bool,
	status_override: Option<PlayerStatus>,
) -> PlayerStatus {
	if !connected {
		return PlayerStatus::Offline;
	}
	status_override.unwrap_or(PlayerStatus::Online)
}

#[cfg(test)]
mod lock_tests {
	use std::time::Duration;

	use uuid::Uuid;

	use super::PlayerLocks;

	#[tokio::test]
	async fn one_player_waits_while_another_goes_ahead() {
		let locks = PlayerLocks::default();
		let (busy, other) = (Uuid::new_v4(), Uuid::new_v4());
		let held = locks.lock(busy).await;

		let waiting = tokio::time::timeout(Duration::from_millis(20), locks.lock(busy)).await;
		assert!(waiting.is_err());
		let free = tokio::time::timeout(Duration::from_millis(20), locks.lock(other)).await;
		assert!(free.is_ok());

		drop(held);
		drop(free);
		let _again = locks.lock(busy).await;
		assert_eq!(locks.0.lock().expect("lock map").len(), 1);
	}
}

#[cfg(test)]
mod status_tests {
	use super::*;

	#[test]
	fn effective_status_respects_connection_then_override() {
		use PlayerStatus::*;
		assert_eq!(effective_status(false, None), Offline);
		assert_eq!(effective_status(false, Some(Online)), Offline);
		assert_eq!(effective_status(true, None), Online);
		assert_eq!(effective_status(true, Some(Offline)), Offline);
		assert_eq!(effective_status(true, Some(Online)), Online);
	}
}

pub type PlaytimeSessions = Arc<RwLock<HashMap<Uuid, PlaytimeSession>>>;

#[derive(Debug, Clone)]
pub struct PlaytimeSession {
	pub player_id: i32,
	/// Row in `play_session` to heartbeat and close.
	pub session_row_id: i64,
	pub last_accounted_at: DateTime<Utc>,
}
