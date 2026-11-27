use std::{
	borrow::Cow,
	collections::HashMap,
	fmt, mem,
};

use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::{
	BodySlot, PartyInviteStatus, PlayerStatus, SessionInviteStatus,
};
use schemars::{JsonSchema, json_schema};
use serde::{Deserialize, Serialize, ser::SerializeStruct as _};
use uuid::Uuid;

use crate::api::{
	v0::bonuses::BonusRef,
	v0::cosmetics::EmoteInfo,
	v0::websocket::colors::SlotColors,
	v1::party::{
		PartyError, Ping, PingReply, PingRequest,
		models::{
			HostedJoinResult, MemberState, Party, PartyInvite, PartyLeftReason,
			PlayDestination, PlayEndReason, PlayReason, PlayResponse,
		},
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
	#[error("Custom colours are not unlocked for cosmetic {0}")]
	CustomColorsNotAllowed(i32),
	#[error("Invalid colours: {0}")]
	InvalidColors(&'static str),
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
	#[error("Invalid ping: {0}")]
	InvalidPing(&'static str),
	#[error("The emote anchor is outside the world")]
	InvalidEmoteAnchor,
	#[error("Invalid shared emote: {0}")]
	InvalidSharedEmote(&'static str),
	#[error("A shared emote request between you and that player is already open")]
	SharedEmotePending,
	#[error("That shared emote request is no longer open")]
	SharedEmoteNotFound,
	#[error("That player is not online")]
	SharedEmotePartnerOffline,
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
		"invalid_ping",
		"shared_emote_pending",
		"shared_emote_not_found",
		"player_offline",
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
			| Self::InvalidPlayTogether(_)
			| Self::InvalidEmoteAnchor
			| Self::InvalidSharedEmote(_)
			| Self::InvalidColors(_) => Self::ERROR_CODES[2],
			Self::UnownedCosmetic(_)
			| Self::UnownedEmote(_)
			| Self::CustomColorsNotAllowed(_) => Self::ERROR_CODES[3],
			Self::NotInParty => Self::ERROR_CODES[4],
			Self::NotPartyLeader => Self::ERROR_CODES[5],
			Self::RateLimited => Self::ERROR_CODES[6],
			Self::PlayTogetherInactive => Self::ERROR_CODES[7],
			Self::InvalidPing(_) => Self::ERROR_CODES[19],
			Self::SharedEmotePending => Self::ERROR_CODES[20],
			Self::SharedEmoteNotFound => Self::ERROR_CODES[21],
			Self::SharedEmotePartnerOffline => Self::ERROR_CODES[22],
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
		#[serde(default)]
		request_id: Option<u64>,
	},
	SetCosmeticColors {
		cosmetic_id: i32,
		colors: Option<SlotColors>,
		#[serde(default = "active_by_default")]
		active: bool,
		#[serde(default)]
		request_id: Option<u64>,
	},
	SetPetPreferences {
		interactions: PetPlayChoice,
	},
	PlayEmote {
		emote_id: i32,
		#[serde(default)]
		anchor: Option<EmoteAnchor>,
		#[serde(default)]
		request_id: Option<u64>,
	},
	StopEmote {
		#[serde(default)]
		emote_id: Option<i32>,
		#[serde(default)]
		start_time: Option<i64>,
	},
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
	SendPing(PingRequest),
	ClearPing {
		id: String,
	},
	ReplyPing {
		id: String,
		owner: String,
		reply: String,
	},
	SharedEmoteRequest {
		emote_id: i32,
		target: Uuid,
		#[serde(default)]
		request_id: Option<u64>,
	},
	SharedEmoteResponse {
		request_id: Uuid,
		accept: bool,
	},
	SharedEmoteStart {
		request_id: Uuid,
		anchor: EmoteAnchor,
	},
	SharedEmoteStop,
	SharedEmoteCancel {
		request_id: Uuid,
	},
}

#[derive(
	Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PetPlay {
	#[default]
	Everyone,
	Friends,
	Off,
}

impl PetPlay {
	pub fn from_switch(interactions: bool) -> Self {
		if interactions { Self::Everyone } else { Self::Off }
	}

	pub fn allows_interactions(self) -> bool {
		self != Self::Off
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PetPlayChoice {
	Switch(bool),
	Setting(PetPlay),
}

impl PetPlayChoice {
	pub fn setting(self) -> PetPlay {
		match self {
			Self::Switch(interactions) => PetPlay::from_switch(interactions),
			Self::Setting(play) => play,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EmoteAnchor {
	pub x: f64,
	pub y: f64,
	pub z: f64,
	pub yaw: f32,
}

impl EmoteAnchor {
	const WORLD_LIMIT: f64 = 30_000_000.0;

	pub fn checked(self) -> Option<Self> {
		let within = |value: f64| value.is_finite() && value.abs() <= Self::WORLD_LIMIT;
		(within(self.x) && within(self.y) && within(self.z) && self.yaw.is_finite()).then(
			|| Self {
				yaw: (self.yaw + 180.0).rem_euclid(360.0) - 180.0,
				..self
			},
		)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct SharedEmote {
	pub emote_id: i32,
	pub players: [Uuid; 2],
	pub anchor: EmoteAnchor,
	pub start_time: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharedEmoteResult {
	Accepted,
	Declined,
	Expired,
	Cancelled,
}

type Settled = Box<dyn FnOnce(bool) + Send + Sync>;

#[derive(Default)]
pub struct WriteReceipt(Option<Settled>);

impl fmt::Debug for WriteReceipt {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_tuple("WriteReceipt").field(&self.0.is_some()).finish()
	}
}

impl WriteReceipt {
	pub fn on_settled(settled: impl FnOnce(bool) + Send + Sync + 'static) -> Self {
		Self(Some(Box::new(settled)))
	}

	pub fn confirm(mut self) {
		if let Some(settled) = self.0.take() {
			settled(true);
		}
	}

	pub fn forget(mut self) {
		self.0.take();
	}
}

impl Drop for WriteReceipt {
	fn drop(&mut self) {
		if let Some(settled) = self.0.take() {
			settled(false);
		}
	}
}

impl ClientBoundPacket {
	pub fn take_write_receipt(&mut self) -> WriteReceipt {
		match self {
			Self::BonusDiscovered { written, .. } => mem::take(written),
			_ => WriteReceipt::default(),
		}
	}
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
		active_emote_starts: HashMap<Uuid, i64>,
		active_emote_anchors: HashMap<Uuid, EmoteAnchor>,
		shared_emotes: HashMap<Uuid, SharedEmote>,
		emote_definitions: HashMap<i32, EmoteInfo>,
		active_emote_definitions: HashMap<Uuid, EmoteInfo>,
		particle_colors: HashMap<Uuid, i32>,
		colors: HashMap<Uuid, HashMap<i32, SlotColors>>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		saved_colors: Option<HashMap<i32, SlotColors>>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		saved_colors_active: Option<Vec<i32>>,
		bonuses: HashMap<Uuid, Vec<BonusRef>>,
		#[serde(default)]
		pet_interactions_off: Vec<Uuid>,
		#[serde(default)]
		pet_interactions_friends: HashMap<Uuid, Vec<Uuid>>,
		/// The subset of subscribed players that currently have a live PolyPlus
		/// session connected. Used to render a "uses PolyPlus" indicator.
		users: Vec<Uuid>,
		rejected: Vec<Uuid>,
		request_id: Option<u64>,
		server_received_time: i64,
		server_time: i64,
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
	PlayerColorsChanged {
		player: Uuid,
		cosmetic_id: i32,
		colors: Option<SlotColors>,
	},
	ParticleColorSaved {
		request_id: u64,
		color: Option<i32>,
	},
	CosmeticColorsSaved {
		request_id: u64,
		cosmetic_id: i32,
		colors: Option<SlotColors>,
		active: bool,
	},
	PlayerBonusesChanged {
		player: Uuid,
		bonuses: Vec<BonusRef>,
	},
	PlayerPetPreferencesChanged {
		player: Uuid,
		interactions: bool,
		pet_play: PetPlay,
		friends: Vec<Uuid>,
	},
	BonusDiscovered {
		bonus_id: i32,
		name: String,
		cosmetic_ids: Vec<i32>,
		#[serde(skip)]
		written: WriteReceipt,
	},
	PlayerEmoteStarted {
		player: Uuid,
		emote_id: i32,
		start_time: Option<i64>,
		#[serde(skip_serializing_if = "Option::is_none")]
		anchor: Option<EmoteAnchor>,
		#[serde(skip_serializing_if = "Option::is_none")]
		request_id: Option<u64>,
		#[serde(skip_serializing_if = "Option::is_none")]
		emote: Option<EmoteInfo>,
	},
	PlayerEmoteStopped {
		player: Uuid,
	},
	SharedEmoteRequested {
		request_id: Uuid,
		from: Uuid,
		emote_id: i32,
		expires_at: i64,
		#[serde(skip_serializing_if = "Option::is_none")]
		emote: Option<EmoteInfo>,
	},
	SharedEmoteRequestSent {
		request_id: Uuid,
		target: Uuid,
		emote_id: i32,
		expires_at: i64,
		#[serde(skip_serializing_if = "Option::is_none")]
		emote: Option<EmoteInfo>,
	},
	SharedEmoteResolved {
		request_id: Uuid,
		result: SharedEmoteResult,
	},
	SharedEmoteStart {
		#[serde(flatten)]
		shared: SharedEmote,
		#[serde(skip_serializing_if = "Option::is_none")]
		emote: Option<EmoteInfo>,
	},
	SharedEmoteStop {
		players: [Uuid; 2],
	},
	OwnershipUpdated {
		player: Uuid,
		cosmetic_ids: Vec<i32>,
		emote_ids: Vec<i32>,
		/// are we revoking stuff
		revoked: bool,
	},
	CatalogUpdated {
		version: i64,
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
	GroupRenamed {
		group_id: i32,
		name: String,
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
	PartyPing {
		from: Uuid,
		sent_at: i64,
		#[serde(flatten)]
		ping: Ping,
	},
	PartyPingCleared {
		id: Uuid,
		from: Uuid,
	},
	PartyPingReply {
		id: Uuid,
		owner: Uuid,
		from: Uuid,
		reply: PingReply,
		sent_at: i64,
	},
	/// An error response from the server
	Error {
		#[serde(flatten)]
		error: WebsocketError,
		request_id: Option<u64>,
	},
}

fn active_by_default() -> bool {
	true
}

impl ServerBoundPacket {
	pub fn request_id(&self) -> Option<u64> {
		match self {
			Self::SubscribePlayers { request_id, .. }
			| Self::PlayEmote { request_id, .. }
			| Self::SetParticleColor { request_id, .. }
			| Self::SetCosmeticColors { request_id, .. }
			| Self::SharedEmoteRequest { request_id, .. } => *request_id,
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::{BTreeMap, HashMap};

	use entities::sea_orm_active_enums::{BodySlot, PlayerStatus};
	use serde_json::{Value, json};
	use uuid::Uuid;

	use super::{
		ClientBoundPacket, EmoteAnchor, PetPlay, ServerBoundPacket, SharedEmote,
		SharedEmoteResult, WebsocketError, WriteReceipt,
	};
	use crate::api::{
		v0::{
			bonuses::BonusRef,
			cosmetics::{CachedAssetInfo, EmoteInfo},
		},
		v1::party::{PartyError, models::HostedJoinResult},
	};

	const ANCHOR: EmoteAnchor = EmoteAnchor {
		x: 1.5,
		y: 64.0,
		z: -3.0,
		yaw: 45.5,
	};

	fn anchor_json() -> Value {
		json!({ "x": 1.5, "y": 64.0, "z": -3.0, "yaw": 45.5 })
	}

	fn retired_emote() -> EmoteInfo {
		EmoteInfo {
			id: 5,
			name: "Retired".to_owned(),
			url: Some("https://objects.example/emotes/retired.zip".to_owned()),
			cached_info: CachedAssetInfo {
				hash: "abc123".to_owned(),
			},
		}
	}

	fn retired_emote_json() -> Value {
		json!({
			"id": 5,
			"name": "Retired",
			"url": "https://objects.example/emotes/retired.zip",
			"hash": "abc123"
		})
	}

	fn snapshot() -> ClientBoundPacket {
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
			rejected: Vec::new(),
			request_id: None,
			server_received_time: 1_700_000_000_100,
			server_time: 1_700_000_000_123,
		}
	}

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
		let mut packet = snapshot();
		let ClientBoundPacket::SubscriptionSnapshot {
			equipped,
			active_emotes,
			active_emote_starts,
			particle_colors,
			colors,
			users,
			request_id,
			..
		} = &mut packet
		else {
			unreachable!()
		};
		*equipped = HashMap::from([(player, HashMap::from([(BodySlot::Cape, 1)]))]);
		*active_emotes = HashMap::from([(player, 6)]);
		*active_emote_starts = HashMap::from([(player, 1_700_000_000_000)]);
		*particle_colors = HashMap::from([(player, 0xFF_0000)]);
		*colors = HashMap::from([(
			player,
			HashMap::from([(1, BTreeMap::from([("fur".to_owned(), 0x12_3456)]))]),
		)]);
		*users = vec![player];
		*request_id = Some(3);

		let serialized = serde_json::to_value(packet).expect("packet should serialize");
		assert_eq!(serialized["type"], "SubscriptionSnapshot");
		assert_eq!(serialized["equipped"][player.to_string()]["cape"], 1);
		assert_eq!(serialized["active_emotes"][player.to_string()], 6);
		assert_eq!(
			serialized["active_emote_starts"][player.to_string()],
			1_700_000_000_000_i64
		);
		assert_eq!(serialized["particle_colors"][player.to_string()], 0xFF_0000);
		assert_eq!(
			serialized["colors"][player.to_string()]["1"]["fur"],
			0x12_3456
		);
		assert_eq!(serialized["users"][0], player.to_string());
		assert_eq!(serialized["rejected"].as_array().map(Vec::len), Some(0));
		assert_eq!(serialized["request_id"], 3);
		assert_eq!(serialized["server_received_time"], 1_700_000_000_100_i64);
		assert_eq!(serialized["server_time"], 1_700_000_000_123_i64);
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

	#[test]
	fn the_snapshot_keeps_older_fields_and_carries_the_new_ones() {
		let empty = serde_json::to_value(snapshot()).expect("packet should serialize");
		let keys = empty.as_object().expect("packet should be an object");
		for key in ["type", "equipped", "active_emotes", "particle_colors", "users", "rejected"] {
			assert!(keys.contains_key(key), "missing {key}");
		}
		assert_eq!(empty["colors"], json!({}));
		assert_eq!(empty["pet_interactions_off"], json!([]));
		assert!(!keys.contains_key("saved_colors") && !keys.contains_key("saved_colors_active"));

		let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
		let shared = SharedEmote { emote_id: 5, players: [a, b], anchor: ANCHOR, start_time: 1 };
		let bonus = BonusRef { id: 5, url: None, hash: "ab".to_owned() };
		let mut packet = snapshot();
		let ClientBoundPacket::SubscriptionSnapshot {
			equipped,
			active_emote_anchors,
			shared_emotes,
			emote_definitions,
			active_emote_definitions,
			saved_colors,
			saved_colors_active,
			bonuses,
			pet_interactions_off,
			pet_interactions_friends,
			..
		} = &mut packet
		else {
			unreachable!()
		};
		*equipped = HashMap::from([(a, HashMap::from([(BodySlot::EffectA, 4), (BodySlot::EffectB, 5)]))]);
		*active_emote_anchors = HashMap::from([(b, ANCHOR)]);
		*shared_emotes = HashMap::from([(a, shared)]);
		*emote_definitions = HashMap::from([(5, retired_emote())]);
		*active_emote_definitions = HashMap::from([(a, retired_emote())]);
		*saved_colors = Some(HashMap::from([(7, BTreeMap::from([("fur".to_owned(), 0x12_3456)]))]));
		*saved_colors_active = Some(vec![7]);
		*bonuses = HashMap::from([(a, vec![bonus])]);
		*pet_interactions_off = vec![a];
		*pet_interactions_friends = HashMap::from([(b, vec![a])]);

		let full = serde_json::to_value(packet).expect("packet should serialize");
		let (a, b) = (a.to_string(), b.to_string());
		assert_eq!(full["equipped"][&a], json!({ "effect_a": 4, "effect_b": 5 }));
		assert_eq!(full["active_emote_anchors"][&b], anchor_json());
		assert_eq!(full["shared_emotes"][&a]["players"], json!([a, b]));
		assert_eq!(full["shared_emotes"][&a]["anchor"], anchor_json());
		assert_eq!(full["emote_definitions"]["5"], retired_emote_json());
		assert_eq!(full["active_emote_definitions"][&a], retired_emote_json());
		assert_eq!(full["saved_colors"], json!({ "7": { "fur": 0x12_3456 } }));
		assert_eq!(full["saved_colors_active"], json!([7]));
		assert_eq!(full["bonuses"][&a], json!([{ "id": 5, "hash": "ab" }]));
		assert_eq!(full["pet_interactions_off"], json!([a]));
		assert_eq!(full["pet_interactions_friends"], json!({ b: [a] }));

		let mut owner = snapshot();
		if let ClientBoundPacket::SubscriptionSnapshot { saved_colors, saved_colors_active, .. } = &mut owner {
			*saved_colors = Some(HashMap::new());
			*saved_colors_active = Some(Vec::new());
		}
		let owner = serde_json::to_value(owner).expect("packet should serialize");
		assert_eq!((&owner["saved_colors"], &owner["saved_colors_active"]), (&json!({}), &json!([])));
	}

	#[test]
	fn new_server_bound_packets_parse() {
		use ServerBoundPacket as P;
		let parse = |raw: &str| serde_json::from_str::<ServerBoundPacket>(raw).expect(raw);
		let nil = Uuid::nil();
		let anchor = r#"{"x":1.5,"y":64,"z":-3,"yaw":45.5}"#;
		let fur = |colors: &Option<BTreeMap<String, i32>>| colors.as_ref().map(|colors| colors["fur"]);

		let equip = parse(r#"{"type":"SetEquippedCosmetic","slot":"effect_b","cosmetic_id":9}"#);
		assert!(matches!(equip, P::SetEquippedCosmetic { slot: BodySlot::EffectB, cosmetic_id: Some(9) }));
		let palette = parse(r#"{"type":"SetCosmeticColors","cosmetic_id":5,"colors":{"fur":1193046,"eyes":0}}"#);
		assert!(matches!(palette, P::SetCosmeticColors { cosmetic_id: 5, colors: Some(ref c), active: true, request_id: None }
			if c.len() == 2 && c["fur"] == 0x12_3456 && c["eyes"] == 0));
		let cleared = parse(r#"{"type":"SetCosmeticColors","cosmetic_id":5,"colors":null}"#);
		assert!(matches!(cleared, P::SetCosmeticColors { cosmetic_id: 5, colors: None, active: true, request_id: None }));
		let saved = parse(r#"{"type":"SetCosmeticColors","cosmetic_id":5,"colors":{"fur":1},"active":false,"request_id":11}"#);
		assert_eq!(saved.request_id(), Some(11));
		assert!(matches!(saved, P::SetCosmeticColors { cosmetic_id: 5, ref colors, active: false, .. } if fur(colors) == Some(1)));
		let particle = parse(r#"{"type":"SetParticleColor","color":-48060}"#);
		assert!(matches!(particle, P::SetParticleColor { color: Some(-48060), request_id: None }));
		let particle = parse(r#"{"type":"SetParticleColor","color":null,"request_id":12}"#);
		assert!(particle.request_id() == Some(12) && matches!(particle, P::SetParticleColor { color: None, .. }));

		assert!(matches!(parse(r#"{"type":"StopEmote"}"#), P::StopEmote { emote_id: None, start_time: None }));
		let stop = parse(r#"{"type":"StopEmote","emote_id":12,"start_time":1700000000000}"#);
		assert!(matches!(stop, P::StopEmote { emote_id: Some(12), start_time: Some(1_700_000_000_000) }));
		let play = parse(r#"{"type":"PlayEmote","emote_id":12,"anchor":null}"#);
		assert!(matches!(play, P::PlayEmote { emote_id: 12, anchor: None, request_id: None }));
		let play = parse(&format!(r#"{{"type":"PlayEmote","emote_id":12,"request_id":5,"anchor":{anchor}}}"#));
		assert!(play.request_id() == Some(5) && matches!(play, P::PlayEmote { emote_id: 12, anchor: Some(a), .. } if a == ANCHOR));

		let request = parse(&format!(r#"{{"type":"SharedEmoteRequest","emote_id":5,"target":"{nil}","request_id":9}}"#));
		assert!(request.request_id() == Some(9) && matches!(request, P::SharedEmoteRequest { emote_id: 5, target, .. } if target == nil));
		assert_eq!(parse(&format!(r#"{{"type":"SharedEmoteRequest","emote_id":5,"target":"{nil}"}}"#)).request_id(), None);
		let response = parse(&format!(r#"{{"type":"SharedEmoteResponse","request_id":"{nil}","accept":false}}"#));
		assert!(matches!(response, P::SharedEmoteResponse { request_id, accept: false } if request_id == nil));
		let start = parse(&format!(r#"{{"type":"SharedEmoteStart","request_id":"{nil}","anchor":{anchor}}}"#));
		assert!(matches!(start, P::SharedEmoteStart { request_id, anchor } if request_id == nil && anchor == ANCHOR));
		let cancel = parse(&format!(r#"{{"type":"SharedEmoteCancel","request_id":"{nil}"}}"#));
		assert!(matches!(cancel, P::SharedEmoteCancel { request_id } if request_id == nil));
		assert!(matches!(parse(r#"{"type":"SharedEmoteStop"}"#), P::SharedEmoteStop));

		for (raw, expected) in [
			("false", PetPlay::Off),
			("true", PetPlay::Everyone),
			(r#""everyone""#, PetPlay::Everyone),
			(r#""friends""#, PetPlay::Friends),
			(r#""off""#, PetPlay::Off),
		] {
			let packet = parse(&format!(r#"{{"type":"SetPetPreferences","interactions":{raw}}}"#));
			assert!(matches!(packet, P::SetPetPreferences { interactions } if interactions.setting() == expected));
		}

		for bad in [
			r#"{"type":"SetCosmeticColors","cosmetic_id":5,"colors":{"fur":"red"}}"#,
			r#"{"type":"SetPetPreferences"}"#,
			r#"{"type":"SetPetPreferences","interactions":"no"}"#,
			r#"{"type":"SetPetPreferences","interactions":"Friends"}"#,
			r#"{"type":"SetPetPreferences","interactions":null}"#,
			r#"{"type":"SharedEmoteCancel"}"#,
		] {
			assert!(serde_json::from_str::<ServerBoundPacket>(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn new_client_bound_packets_serialize() {
		let (a, b, id) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
		let (a_s, b_s, id_s) = (a.to_string(), b.to_string(), id.to_string());
		let shared = SharedEmote { emote_id: 5, players: [a, b], anchor: ANCHOR, start_time: 7 };
		let started = |anchor, request_id, emote| ClientBoundPacket::PlayerEmoteStarted {
			player: a,
			emote_id: 12,
			start_time: Some(7),
			anchor,
			request_id,
			emote,
		};
		let fur = || Some(BTreeMap::from([("fur".to_owned(), 0x12_3456)]));
		for (packet, expected) in [
			(
				ClientBoundPacket::CatalogUpdated { version: 1_762_790_400_000 },
				json!({ "type": "CatalogUpdated", "version": 1_762_790_400_000_i64 }),
			),
			(
				ClientBoundPacket::PlayerCosmeticEquipped {
					player: a,
					slot: BodySlot::EffectA,
					cosmetic_id: Some(4),
				},
				json!({ "type": "PlayerCosmeticEquipped", "player": a_s, "slot": "effect_a", "cosmetic_id": 4 }),
			),
			(
				ClientBoundPacket::PlayerColorsChanged { player: a, cosmetic_id: 7, colors: fur() },
				json!({ "type": "PlayerColorsChanged", "player": a_s, "cosmetic_id": 7, "colors": { "fur": 0x12_3456 } }),
			),
			(
				ClientBoundPacket::PlayerColorsChanged { player: a, cosmetic_id: 7, colors: None },
				json!({ "type": "PlayerColorsChanged", "player": a_s, "cosmetic_id": 7, "colors": null }),
			),
			(
				ClientBoundPacket::CosmeticColorsSaved { request_id: 11, cosmetic_id: 5, colors: fur(), active: false },
				json!({ "type": "CosmeticColorsSaved", "request_id": 11, "cosmetic_id": 5, "colors": { "fur": 0x12_3456 }, "active": false }),
			),
			(
				ClientBoundPacket::ParticleColorSaved { request_id: 12, color: None },
				json!({ "type": "ParticleColorSaved", "request_id": 12, "color": null }),
			),
			(
				ClientBoundPacket::PlayerPetPreferencesChanged {
					player: a,
					interactions: true,
					pet_play: PetPlay::Friends,
					friends: vec![b],
				},
				json!({ "type": "PlayerPetPreferencesChanged", "player": a_s, "interactions": true, "pet_play": "friends", "friends": [b_s] }),
			),
			(
				ClientBoundPacket::PlayerBonusesChanged {
					player: a,
					bonuses: vec![BonusRef { id: 3, url: None, hash: "cd".to_owned() }],
				},
				json!({ "type": "PlayerBonusesChanged", "player": a_s, "bonuses": [{ "id": 3, "hash": "cd" }] }),
			),
			(
				ClientBoundPacket::BonusDiscovered {
					bonus_id: 3,
					name: "Night Watch".to_owned(),
					cosmetic_ids: vec![10, 41],
					written: WriteReceipt::default(),
				},
				json!({ "type": "BonusDiscovered", "bonus_id": 3, "name": "Night Watch", "cosmetic_ids": [10, 41] }),
			),
			(
				started(None, None, None),
				json!({ "type": "PlayerEmoteStarted", "player": a_s, "emote_id": 12, "start_time": 7 }),
			),
			(
				started(Some(ANCHOR), Some(5), Some(retired_emote())),
				json!({
					"type": "PlayerEmoteStarted", "player": a_s, "emote_id": 12, "start_time": 7,
					"anchor": anchor_json(), "request_id": 5, "emote": retired_emote_json()
				}),
			),
			(
				ClientBoundPacket::PlayerEmoteStopped { player: a },
				json!({ "type": "PlayerEmoteStopped", "player": a_s }),
			),
			(
				ClientBoundPacket::SharedEmoteStart { shared, emote: Some(retired_emote()) },
				json!({
					"type": "SharedEmoteStart", "emote_id": 5, "players": [a_s, b_s],
					"anchor": anchor_json(), "start_time": 7, "emote": retired_emote_json()
				}),
			),
			(
				ClientBoundPacket::SharedEmoteRequested {
					request_id: id,
					from: a,
					emote_id: 5,
					expires_at: 10,
					emote: None,
				},
				json!({ "type": "SharedEmoteRequested", "request_id": id_s, "from": a_s, "emote_id": 5, "expires_at": 10 }),
			),
			(
				ClientBoundPacket::SharedEmoteRequestSent {
					request_id: id,
					target: b,
					emote_id: 5,
					expires_at: 10,
					emote: Some(retired_emote()),
				},
				json!({
					"type": "SharedEmoteRequestSent", "request_id": id_s, "target": b_s, "emote_id": 5,
					"expires_at": 10, "emote": retired_emote_json()
				}),
			),
			(
				ClientBoundPacket::SharedEmoteStop { players: [a, b] },
				json!({ "type": "SharedEmoteStop", "players": [a_s, b_s] }),
			),
			(
				ClientBoundPacket::SharedEmoteResolved { request_id: id, result: SharedEmoteResult::Declined },
				json!({ "type": "SharedEmoteResolved", "request_id": id_s, "result": "declined" }),
			),
		] {
			assert_eq!(serde_json::to_value(packet).expect("packet should serialize"), expected);
		}
	}

	#[test]
	fn new_errors_have_known_codes() {
		for (error, code) in [
			(WebsocketError::InvalidColors("x"), "bad_request"),
			(WebsocketError::CustomColorsNotAllowed(1), "not_owned"),
			(WebsocketError::InvalidEmoteAnchor, "bad_request"),
			(WebsocketError::SharedEmotePending, "shared_emote_pending"),
			(WebsocketError::SharedEmoteNotFound, "shared_emote_not_found"),
			(WebsocketError::SharedEmotePartnerOffline, "player_offline"),
			(WebsocketError::InvalidSharedEmote("x"), "bad_request"),
		] {
			assert_eq!(error.error_code(), code);
			assert!(WebsocketError::ERROR_CODES.contains(&code));
		}
	}

	#[test]
	fn pet_play_maps_the_old_switch() {
		assert_eq!(PetPlay::default(), PetPlay::Everyone);
		assert_eq!(PetPlay::from_switch(true), PetPlay::Everyone);
		assert_eq!(PetPlay::from_switch(false), PetPlay::Off);
		assert!(PetPlay::Everyone.allows_interactions() && PetPlay::Friends.allows_interactions());
		assert!(!PetPlay::Off.allows_interactions());
	}

	#[test]
	fn anchors_must_be_finite_and_inside_the_world() {
		let edge = EmoteAnchor { x: 29_999_999.5, y: -64.0, z: -30_000_000.0, yaw: 270.0 };
		let checked = edge.checked().expect("anchor should pass");
		assert_eq!((checked.x, checked.y, checked.z), (edge.x, edge.y, edge.z));
		assert!((checked.yaw + 90.0).abs() < 1.0e-4);
		let behind = EmoteAnchor { yaw: -540.0, ..edge }.checked().expect("anchor should pass");
		assert!((behind.yaw + 180.0).abs() < 1.0e-4);

		let origin = EmoteAnchor { x: 0.0, y: 64.0, z: 0.0, yaw: 0.0 };
		for bad in [
			EmoteAnchor { x: 30_000_000.5, ..origin },
			EmoteAnchor { y: -1.0e9, ..origin },
			EmoteAnchor { z: f64::NAN, ..origin },
			EmoteAnchor { x: f64::INFINITY, ..origin },
			EmoteAnchor { yaw: f32::NAN, ..origin },
			EmoteAnchor { yaw: f32::NEG_INFINITY, ..origin },
		] {
			assert_eq!(bad.checked(), None, "{bad:?} should be rejected");
		}
	}
}
