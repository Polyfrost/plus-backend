use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
};

use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::BodySlot;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::api::v0::{account::ClientKind, websocket::structs::ClientBoundPacket};

pub type ConnectionId = Uuid;

/// Everything the websocket layer keeps in memory, shared across connections.
#[derive(Debug, Clone, Default)]
pub struct RealtimeState {
	pub connections: Arc<RwLock<HashMap<ConnectionId, RealtimeConnection>>>,
	pub connections_by_owner: Arc<RwLock<HashMap<Uuid, HashSet<ConnectionId>>>>,
	pub player_runtime: Arc<RwLock<HashMap<Uuid, PlayerRuntimeState>>>,
	pub watchers: Arc<RwLock<HashMap<Uuid, HashSet<ConnectionId>>>>,
	pub playtime: PlaytimeSessions,
}

impl RealtimeState {
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
}

/// Cosmetic state a player is currently broadcasting to watchers.
#[derive(Debug, Clone, Default)]
pub struct PlayerRuntimeState {
	pub equipped: HashMap<BodySlot, i32>,
	pub active_emote: Option<i32>,
	pub particle_color: Option<i32>,
}

pub type PlaytimeSessions = Arc<RwLock<HashMap<Uuid, PlaytimeSession>>>;

#[derive(Debug, Clone)]
pub struct PlaytimeSession {
	pub player_id: i32,
	/// Row in `play_session` to heartbeat and close.
	pub session_row_id: i64,
	pub last_accounted_at: DateTime<Utc>,
}
