use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
};

use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::{BodySlot, PlayerStatus};
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
	/// Each player's live connections, with the kind of client holding each.
	pub connections_by_owner: Arc<RwLock<HashMap<Uuid, OwnerConnections>>>,
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

	/// How `owner` is connected right now.
	pub async fn reach(&self, owner: Uuid) -> Reach {
		Reach::of(self.connections_by_owner.read().await.get(&owner))
	}
}

/// One player's live connections.
pub type OwnerConnections = HashMap<ConnectionId, ClientKind>;

/// How a player is connected, across all of their live connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
	Disconnected,
	/// Every live connection is the OneClient mobile app.
	MobileOnly,
	/// At least one connection is the game, a launcher or the dashboard.
	Connected,
}

impl Reach {
	pub fn of(connections: Option<&OwnerConnections>) -> Self {
		match connections {
			None => Self::Disconnected,
			Some(connections) if connections.is_empty() => Self::Disconnected,
			Some(connections)
				if connections.values().all(|kind| *kind == ClientKind::Mobile) =>
			{
				Self::MobileOnly
			}
			Some(_) => Self::Connected,
		}
	}
}

/// The status other players see. Unlike [`PlayerStatus`], which is only what
/// a player can pick, this also covers what their connections imply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
	Online,
	/// Connected, but only through the OneClient mobile app.
	Mobile,
	Offline,
}

impl Presence {
	/// The plain online flag older clients read.
	pub fn is_online(self) -> bool {
		self != Self::Offline
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
	/// The status the player picked, `None` meaning derive it from their connection.
	pub status_override: Option<PlayerStatus>,
}

/// The status other players see: offline while disconnected or when the
/// player picked offline, mobile while only the mobile app is connected,
/// otherwise online.
pub fn effective_status(
	reach: Reach,
	status_override: Option<PlayerStatus>,
) -> Presence {
	match (reach, status_override) {
		(Reach::Disconnected, _) | (_, Some(PlayerStatus::Offline)) => Presence::Offline,
		(Reach::MobileOnly, _) => Presence::Mobile,
		(Reach::Connected, _) => Presence::Online,
	}
}

#[cfg(test)]
mod status_tests {
	use super::*;

	#[test]
	fn effective_status_respects_connection_then_override() {
		use PlayerStatus::{Offline, Online};
		use Reach::{Connected, Disconnected, MobileOnly};
		assert_eq!(effective_status(Disconnected, None), Presence::Offline);
		assert_eq!(effective_status(Disconnected, Some(Online)), Presence::Offline);
		assert_eq!(effective_status(Connected, None), Presence::Online);
		assert_eq!(effective_status(Connected, Some(Offline)), Presence::Offline);
		assert_eq!(effective_status(Connected, Some(Online)), Presence::Online);
		assert_eq!(effective_status(MobileOnly, None), Presence::Mobile);
		assert_eq!(effective_status(MobileOnly, Some(Online)), Presence::Mobile);
		assert_eq!(effective_status(MobileOnly, Some(Offline)), Presence::Offline);
	}

	#[test]
	fn reach_is_mobile_only_while_every_connection_is_mobile() {
		let connections = |kinds: &[ClientKind]| -> OwnerConnections {
			kinds.iter().map(|kind| (Uuid::new_v4(), *kind)).collect()
		};

		assert_eq!(Reach::of(None), Reach::Disconnected);
		assert_eq!(Reach::of(Some(&connections(&[]))), Reach::Disconnected);
		assert_eq!(
			Reach::of(Some(&connections(&[ClientKind::Mobile, ClientKind::Mobile]))),
			Reach::MobileOnly
		);
		for other in [ClientKind::Game, ClientKind::Other] {
			assert_eq!(Reach::of(Some(&connections(&[other]))), Reach::Connected);
			assert_eq!(
				Reach::of(Some(&connections(&[ClientKind::Mobile, other]))),
				Reach::Connected
			);
		}
	}

	#[test]
	fn presence_serializes_in_snake_case() {
		assert_eq!(serde_json::to_value(Presence::Mobile).unwrap(), "mobile");
		assert!(Presence::Mobile.is_online());
		assert!(!Presence::Offline.is_online());
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
