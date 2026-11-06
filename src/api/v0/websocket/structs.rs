use std::{borrow::Cow, collections::HashMap};

use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::{
	BodySlot, PartyInviteStatus, PlayerStatus, SessionInviteStatus,
};
use schemars::{JsonSchema, json_schema};
use serde::{Deserialize, Serialize, ser::SerializeStruct as _};
use uuid::Uuid;

use crate::api::v1::party::{
	PartyError,
	models::{
		HostedJoinResult, MemberState, Party, PartyInvite, PartyLeftReason,
		PlayDestination, PlayEndReason, PlayReason, PlayResponse,
	},
};

#[derive(Debug, thiserror::Error)]
pub enum WebsocketError {
	#[error("A fatal websocket connection error")]
	Fatal(#[from] axum::Error),
	#[error("Unable to query database: {0}")]
	DatabaseQuery(#[from] sea_orm::error::DbErr),
	#[error("Unable to serialize response: {0}")]
	Serialization(#[from] serde_json::Error),
	#[error("Unable to parse request: {0}")]
	Deserialization(serde_json::Error),
	#[error("Player does not own cosmetic {0}")]
	UnownedCosmetic(i32),
	#[error("Cosmetic {cosmetic_id} is not allowed in slot {slot:?}")]
	InvalidSlot { slot: BodySlot, cosmetic_id: i32 },
	#[error("Player does not own emote {0}")]
	UnownedEmote(i32),
	#[error("Too many players in one request (max {limit})")]
	TooManyPlayersInRequest { limit: usize },
	#[error("Invalid party state: {0}")]
	InvalidPartyState(&'static str),
	#[error("Invalid Play together: {0}")]
	InvalidPlayTogether(&'static str),
	#[error("You are not in a party")]
	NotInParty,
	#[error("Only the party leader may do that")]
	NotPartyLeader,
	#[error("Too many requests, please slow down")]
	RateLimited,
	#[error("That Play together is no longer active")]
	PlayTogetherInactive,
	#[error(transparent)]
	Party(PartyError),
}

impl From<PartyError> for WebsocketError {
	fn from(error: PartyError) -> Self {
		match error {
			PartyError::Database(error) => Self::DatabaseQuery(error),
			PartyError::NotInParty => Self::NotInParty,
			PartyError::RateLimited(_) => Self::RateLimited,
			error => Self::Party(error),
		}
	}
}

impl WebsocketError {
	const ERROR_CODES: &[&str] = &[
		"fatal",
		"internal_server_error",
		"bad_request",
		"not_owned",
		"not_in_party",
		"not_leader",
		"rate_limited",
		"play_together_inactive",
		"session_not_found",
		"own_session",
		"host_not_in_party",
		"host_offline",
		"blocked",
		"already_in_session",
		"already_invited_to_session",
		"join_request_pending",
		"join_request_not_found",
		"member_not_found",
		"not_host",
	];

	/// Whether this error describes an internal failure whose message must not
	/// be sent to the client.
	pub fn is_internal(&self) -> bool {
		match self {
			Self::Fatal(_) | Self::DatabaseQuery(_) | Self::Serialization(_) => true,
			Self::Party(error) => error.is_internal(),
			_ => false,
		}
	}

	fn public_message(&self) -> Cow<'_, str> {
		if self.is_internal() {
			Cow::Borrowed(crate::api::INTERNAL_MESSAGE)
		} else {
			Cow::Owned(self.to_string())
		}
	}

	pub fn error_code(&self) -> &'static str {
		match self {
			Self::Fatal(_) => Self::ERROR_CODES[0],
			Self::DatabaseQuery(_) | Self::Serialization(_) => Self::ERROR_CODES[1],
			Self::Deserialization(_)
			| Self::InvalidSlot { .. }
			| Self::TooManyPlayersInRequest { .. }
			| Self::InvalidPartyState(_)
			| Self::InvalidPlayTogether(_) => Self::ERROR_CODES[2],
			Self::UnownedCosmetic(_) | Self::UnownedEmote(_) => Self::ERROR_CODES[3],
			Self::NotInParty => Self::ERROR_CODES[4],
			Self::NotPartyLeader => Self::ERROR_CODES[5],
			Self::RateLimited => Self::ERROR_CODES[6],
			Self::PlayTogetherInactive => Self::ERROR_CODES[7],
			Self::Party(error) if error.is_internal() => Self::ERROR_CODES[1],
			Self::Party(error) => error.code(),
		}
	}
}

impl Serialize for WebsocketError {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut state = serializer.serialize_struct("WebsocketError", 2)?;
		state.serialize_field("error_code", self.error_code())?;
		state.serialize_field("message", &self.public_message())?;
		state.end()
	}
}

impl JsonSchema for WebsocketError {
	fn schema_name() -> Cow<'static, str> {
		Cow::Borrowed("WebsocketError")
	}

	fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
		json_schema!({
			"type": "object",
			"properties": {
				"error_code": {
					"enum": WebsocketError::ERROR_CODES,
					"description": "The machine-readable unique error code",
					"example": "internal_server_error"
				},
				"message": {
					"type": "string",
					"description": "The human-readable error message"
				}
			}
		})
	}
}

/// A JSON object that a client can send in the websocket connection
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase", tag = "type")]
pub enum ServerBoundPacket {
	/// Fetches active cosmetics for a list of players in bulk
	GetActiveCosmetics {
		/// An array of player UUIDs to include in the bulk lookup
		players: Vec<Uuid>,
	},
	/// Subscribe to cosmetic/emote updates for nearby players (render distance).
	SubscribePlayers {
		/// Player UUIDs to watch. Capped per request and per connection total.
		players: Vec<Uuid>,
		#[serde(default)]
		request_id: Option<u64>,
	},
	UnsubscribePlayers {
		players: Vec<Uuid>,
	},
	SetEquippedCosmetic {
		slot: BodySlot,
		cosmetic_id: Option<i32>,
	},
	SetParticleColor {
		color: Option<i32>,
	},
	PlayEmote {
		emote_id: i32,
	},
	StopEmote,
	SetPartyState {
		state: MemberState,
	},
	StartPlayTogether {
		destination: PlayDestination,
	},
	CancelPlayTogether {
		id: Uuid,
	},
	PlayTogetherResponse {
		id: Uuid,
		response: PlayResponse,
		#[serde(default)]
		reason: Option<PlayReason>,
	},
	RequestHostedJoin {
		session_id: Uuid,
	},
	HostedJoinResponse {
		request_id: Uuid,
		allow: bool,
	},
}

/// A JSON object that the server will send to the client in the websocket
/// connection
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "PascalCase", tag = "type")]
pub enum ClientBoundPacket {
	/// Information on player UUIDs and what cosmetics they own, sent in
	/// response to [ServerBoundPacket::GetActiveCosmetics]
	CosmeticsInfo {
		/// An object mapping player UUIDs to a list of their active cosmetic
		/// IDs. These cosmetic IDs should be resolved seperately, as no other
		/// information is given in this response.
		///
		/// Any players without active cosmetics are not returned in this
		/// object.
		#[schemars(example = HashMap::from([("424ef6d0-4774-4f8c-8bef-8f62ebdac9c0", [1,2])]))]
		cosmetics: HashMap<Uuid, Vec<i32>>,
	},
	SubscriptionSnapshot {
		equipped: HashMap<Uuid, HashMap<BodySlot, i32>>,
		active_emotes: HashMap<Uuid, i32>,
		particle_colors: HashMap<Uuid, i32>,
		/// The subset of subscribed players that currently have a live PolyPlus
		/// session connected. Used to render a "uses PolyPlus" indicator.
		users: Vec<Uuid>,
		rejected: Vec<Uuid>,
		request_id: Option<u64>,
	},
	/// A subscribed player's PolyPlus session came online or went offline.
	PlayerPresence {
		player: Uuid,
		/// Kept for older clients
		online: bool,
		status: PlayerStatus,
	},
	PlayerCosmeticEquipped {
		player: Uuid,
		slot: BodySlot,
		cosmetic_id: Option<i32>,
	},
	PlayerParticleColorChanged {
		player: Uuid,
		color: Option<i32>,
	},
	PlayerEmoteStarted {
		player: Uuid,
		emote_id: i32,
	},
	PlayerEmoteStopped {
		player: Uuid,
	},
	OwnershipUpdated {
		player: Uuid,
		cosmetic_ids: Vec<i32>,
		emote_ids: Vec<i32>,
		/// are we revoking stuff
		revoked: bool,
	},
	FriendRequestReceived {
		request_id: i32,
		sender: Uuid,
	},
	FriendRequestUpdated {
		request_id: i32,
		status: String,
	},
	FriendRemoved {
		player: Uuid,
	},
	GroupMessageReceived {
		group_id: i32,
		message_id: i64,
		sender: Uuid,
		content: String,
		session_invite_id: Option<i32>,
		session_invite_status: Option<SessionInviteStatus>,
	},
	GroupMessageEdited {
		group_id: i32,
		message_id: i64,
		content: String,
		session_invite_status: Option<SessionInviteStatus>,
	},
	GroupMessageDeleted {
		group_id: i32,
		message_id: i64,
	},
	GlobalChatMessageReceived {
		message_id: i64,
		sender: Uuid,
		content: String,
	},
	SessionInviteReceived {
		invite_id: i32,
		session_id: Uuid,
		sender: Uuid,
	},
	SessionInviteUpdated {
		invite_id: i32,
		status: String,
	},
	PartyUpdated {
		party: Party,
	},
	PartyMemberStateChanged {
		party_id: Uuid,
		player: Uuid,
		state: MemberState,
	},
	PartyLeft {
		party_id: Uuid,
		reason: PartyLeftReason,
	},
	PartyInviteReceived {
		invite: PartyInvite,
	},
	PartyInviteUpdated {
		invite_id: Uuid,
		status: PartyInviteStatus,
	},
	PlayTogetherStarted {
		id: Uuid,
		party_id: Uuid,
		leader: Uuid,
		destination: PlayDestination,
		starts_at: DateTime<Utc>,
		session_id: Option<Uuid>,
	},
	PlayTogetherUpdated {
		id: Uuid,
		player: Uuid,
		response: PlayResponse,
		reason: Option<PlayReason>,
		session_id: Option<Uuid>,
	},
	PlayTogetherEnded {
		id: Uuid,
		reason: PlayEndReason,
	},
	HostedJoinRequested {
		request_id: Uuid,
		session_id: Uuid,
		player: Uuid,
		expires_at: DateTime<Utc>,
	},
	HostedJoinRequestSent {
		request_id: Uuid,
		session_id: Uuid,
		host: Uuid,
		expires_at: DateTime<Utc>,
	},
	HostedJoinResolved {
		request_id: Uuid,
		session_id: Uuid,
		result: HostedJoinResult,
		invite_id: Option<i32>,
	},
	/// An error response from the server
	Error {
		#[serde(flatten)]
		error: WebsocketError,
		request_id: Option<u64>,
	},
}

impl ServerBoundPacket {
	pub fn request_id(&self) -> Option<u64> {
		match self {
			Self::SubscribePlayers { request_id, .. } => *request_id,
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use entities::sea_orm_active_enums::{BodySlot, PlayerStatus};
	use uuid::Uuid;

	use super::{ClientBoundPacket, ServerBoundPacket};
	use crate::api::v1::party::{PartyError, models::HostedJoinResult};

	#[test]
	fn parses_slot_based_equipment_update() {
		let packet: ServerBoundPacket = serde_json::from_str(
			r#"{"type":"SetEquippedCosmetic","slot":"cape","cosmetic_id":42}"#,
		)
		.expect("packet should parse");

		match packet {
			ServerBoundPacket::SetEquippedCosmetic { slot, cosmetic_id } => {
				assert_eq!(slot, BodySlot::Cape);
				assert_eq!(cosmetic_id, Some(42));
			}
			_ => panic!("unexpected packet variant"),
		}
	}

	#[test]
	fn parses_player_subscription() {
		let player = Uuid::nil();
		let packet: ServerBoundPacket = serde_json::from_str(&format!(
			r#"{{"type":"SubscribePlayers","players":["{player}"]}}"#
		))
		.expect("packet should parse");

		match packet {
			ServerBoundPacket::SubscribePlayers {
				players,
				request_id,
			} => {
				assert_eq!(players, vec![player]);
				assert_eq!(request_id, None);
			}
			_ => panic!("unexpected packet variant"),
		}
	}

	#[test]
	fn parses_player_subscription_with_request_id() {
		let player = Uuid::nil();
		let packet: ServerBoundPacket = serde_json::from_str(&format!(
			r#"{{"type":"SubscribePlayers","players":["{player}"],"request_id":7}}"#
		))
		.expect("packet should parse");

		assert_eq!(packet.request_id(), Some(7));
	}

	#[test]
	fn serializes_subscription_snapshot_packet() {
		let player = Uuid::nil();
		let packet = ClientBoundPacket::SubscriptionSnapshot {
			equipped: HashMap::from([(player, HashMap::from([(BodySlot::Cape, 1)]))]),
			active_emotes: HashMap::from([(player, 6)]),
			particle_colors: HashMap::from([(player, 0xFF_0000)]),
			users: vec![player],
			rejected: Vec::new(),
			request_id: Some(3),
		};

		let serialized = serde_json::to_value(packet).expect("packet should serialize");
		assert_eq!(serialized["type"], "SubscriptionSnapshot");
		assert_eq!(serialized["equipped"][player.to_string()]["cape"], 1);
		assert_eq!(serialized["users"][0], player.to_string());
		assert_eq!(serialized["rejected"].as_array().map(Vec::len), Some(0));
		assert_eq!(serialized["request_id"], 3);
	}

	#[test]
	fn serializes_error_packet_with_request_id() {
		let packet = ClientBoundPacket::Error {
			error: super::WebsocketError::TooManyPlayersInRequest { limit: 64 },
			request_id: Some(9),
		};

		let serialized = serde_json::to_value(packet).expect("packet should serialize");
		assert_eq!(serialized["type"], "Error");
		assert_eq!(serialized["error_code"], "bad_request");
		assert_eq!(serialized["request_id"], 9);
	}

	#[test]
	fn parses_hosted_join_packets() {
		let id = Uuid::nil();
		let packet: ServerBoundPacket = serde_json::from_str(&format!(
			r#"{{"type":"RequestHostedJoin","session_id":"{id}"}}"#
		))
		.expect("packet should parse");
		assert!(matches!(
			packet,
			ServerBoundPacket::RequestHostedJoin { session_id } if session_id == id
		));

		let packet: ServerBoundPacket = serde_json::from_str(&format!(
			r#"{{"type":"HostedJoinResponse","request_id":"{id}","allow":true}}"#
		))
		.expect("packet should parse");
		assert!(matches!(
			packet,
			ServerBoundPacket::HostedJoinResponse { request_id, allow: true } if request_id == id
		));
	}

	#[test]
	fn serializes_hosted_join_resolved_packet() {
		let id = Uuid::nil();
		let packet = ClientBoundPacket::HostedJoinResolved {
			request_id: id,
			session_id: id,
			result: HostedJoinResult::Allowed,
			invite_id: Some(4),
		};

		let serialized = serde_json::to_value(packet).expect("packet should serialize");
		assert_eq!(serialized["type"], "HostedJoinResolved");
		assert_eq!(serialized["request_id"], id.to_string());
		assert_eq!(serialized["result"], "allowed");
		assert_eq!(serialized["invite_id"], 4);
	}

	#[test]
	fn party_errors_keep_their_code() {
		let error = super::WebsocketError::from(PartyError::HostOffline);
		assert_eq!(error.error_code(), "host_offline");
		assert!(super::WebsocketError::ERROR_CODES.contains(&error.error_code()));
	}

	#[test]
	fn serializes_player_presence_packet() {
		let player = Uuid::nil();
		let packet = ClientBoundPacket::PlayerPresence {
			player,
			online: true,
			status: PlayerStatus::Online,
		};

		let serialized = serde_json::to_value(packet).expect("packet should serialize");
		assert_eq!(serialized["type"], "PlayerPresence");
		assert_eq!(serialized["player"], player.to_string());
		assert_eq!(serialized["online"], true);
		assert_eq!(serialized["status"], "online");
	}
}
