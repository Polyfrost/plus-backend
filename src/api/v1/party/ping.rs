use chrono::Utc;
use entities::user;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::roster::{lookup_party_id, member_uuids};
use crate::api::{
	ApiState,
	v0::websocket::{
		send_to_owner,
		structs::{ClientBoundPacket, WebsocketError},
	},
};

const MAX_COORDINATE: f64 = 30_000_000.0;
const MAX_DIMENSION_LENGTH: usize = 64;
const MAX_TARGET_FIELD_LENGTH: usize = 64;
const MAX_LABEL_LENGTH: usize = 48;
const SERVER_KEY_LENGTH: usize = 64;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PingRequest {
	pub id: String,
	pub server: String,
	pub dimension: String,
	pub x: f64,
	pub y: f64,
	pub z: f64,
	pub kind: String,
	#[serde(default)]
	pub target: Option<PingTargetRequest>,
	#[serde(default)]
	pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PingTargetRequest {
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default)]
	pub id: Option<String>,
	#[serde(default)]
	pub entity: Option<String>,
	#[serde(default)]
	pub entity_id: Option<i32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Ping {
	pub id: Uuid,
	pub server: String,
	pub dimension: String,
	pub x: f64,
	pub y: f64,
	pub z: f64,
	pub kind: PingKind,
	pub target: Option<PingTarget>,
	pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PingTarget {
	#[serde(rename = "type")]
	pub kind: PingTargetKind,
	pub id: Option<String>,
	pub entity: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub entity_id: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PingKind {
	Go,
	Enemy,
	Look,
	Loot,
	Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PingTargetKind {
	Block,
	Entity,
	Player,
	Item,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PingReply {
	Seen,
	Coming,
	Cant,
	None,
}

impl PingRequest {
	pub fn validate(self) -> Result<Ping, &'static str> {
		let id = parse_ping_id(&self.id)?;
		if self.server.len() != SERVER_KEY_LENGTH
			|| !self.server.bytes().all(|byte| byte.is_ascii_hexdigit())
		{
			return Err("server must be 64 hex characters");
		}
		if self.dimension.chars().count() > MAX_DIMENSION_LENGTH {
			return Err("dimension is too long");
		}
		if ![self.x, self.y, self.z]
			.iter()
			.all(|value| value.is_finite() && value.abs() <= MAX_COORDINATE)
		{
			return Err("coordinates are out of range");
		}
		let kind = parse_enum(&self.kind).ok_or("unknown ping kind")?;
		let target = self.target.map(PingTargetRequest::validate).transpose()?;
		let label = match self.label {
			Some(label) => sanitize_label(&label)?,
			None => None,
		};

		Ok(Ping {
			id,
			server: self.server,
			dimension: self.dimension,
			x: self.x,
			y: self.y,
			z: self.z,
			kind,
			target,
			label,
		})
	}
}

impl PingTargetRequest {
	fn validate(self) -> Result<PingTarget, &'static str> {
		let kind = parse_enum(&self.kind).ok_or("unknown target type")?;
		let too_long = |field: &Option<String>| {
			field
				.as_ref()
				.is_some_and(|value| value.chars().count() > MAX_TARGET_FIELD_LENGTH)
		};
		if too_long(&self.id) || too_long(&self.entity) {
			return Err("target field is too long");
		}

		Ok(PingTarget {
			kind,
			id: self.id,
			entity: self.entity,
			entity_id: self.entity_id,
		})
	}
}

fn parse_ping_id(id: &str) -> Result<Uuid, &'static str> {
	Uuid::parse_str(id).map_err(|_| "id must be a UUID")
}

fn parse_enum<T: for<'de> Deserialize<'de>>(value: &str) -> Option<T> {
	T::deserialize(
		serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value),
	)
	.ok()
}

fn sanitize_label(label: &str) -> Result<Option<String>, &'static str> {
	let label = label
		.chars()
		.filter(|character| !character.is_control())
		.collect::<String>();
	let label = label.trim();
	if label.chars().count() > MAX_LABEL_LENGTH {
		return Err("label is too long");
	}

	Ok((!label.is_empty()).then(|| label.to_owned()))
}

async fn party_recipients(
	state: &ApiState,
	player: &user::Model,
) -> Result<Vec<Uuid>, WebsocketError> {
	let party = lookup_party_id(state, player.minecraft_uuid, Some(player.id))
		.await?
		.ok_or(WebsocketError::NotInParty)?;
	let members = match state.party.roster(party).await {
		Some(roster) => roster,
		None => member_uuids(state, party).await?,
	};

	Ok(members
		.into_iter()
		.filter(|member| *member != player.minecraft_uuid)
		.collect())
}

pub(in crate::api) async fn send_ping(
	state: &ApiState,
	player: &user::Model,
	request: PingRequest,
) -> Result<(), WebsocketError> {
	if state.ping_limit.check(player.id).await.is_some() {
		return Err(WebsocketError::RateLimited);
	}
	let ping = request.validate().map_err(WebsocketError::InvalidPing)?;

	let from = player.minecraft_uuid;
	let sent_at = Utc::now().timestamp_millis();
	for member in party_recipients(state, player).await? {
		send_to_owner(state, member, || ClientBoundPacket::PartyPing {
			from,
			sent_at,
			ping: ping.clone(),
		})
		.await;
	}

	Ok(())
}

pub(in crate::api) async fn clear_ping(
	state: &ApiState,
	player: &user::Model,
	id: &str,
) -> Result<(), WebsocketError> {
	if state.ping_clear_limit.check(player.id).await.is_some() {
		return Err(WebsocketError::RateLimited);
	}
	let id = parse_ping_id(id).map_err(WebsocketError::InvalidPing)?;

	let from = player.minecraft_uuid;
	for member in party_recipients(state, player).await? {
		send_to_owner(state, member, || ClientBoundPacket::PartyPingCleared {
			id,
			from,
		})
		.await;
	}

	Ok(())
}

pub(in crate::api) async fn reply_to_ping(
	state: &ApiState,
	player: &user::Model,
	id: &str,
	owner: &str,
	reply: &str,
) -> Result<(), WebsocketError> {
	if state.ping_reply_limit.check(player.id).await.is_some() {
		return Err(WebsocketError::RateLimited);
	}
	let id = parse_ping_id(id).map_err(WebsocketError::InvalidPing)?;
	let owner = Uuid::parse_str(owner)
		.map_err(|_| WebsocketError::InvalidPing("owner must be a UUID"))?;
	if owner == player.minecraft_uuid {
		return Err(WebsocketError::InvalidPing(
			"you cannot reply to your own ping",
		));
	}
	let reply = parse_enum::<PingReply>(reply)
		.ok_or(WebsocketError::InvalidPing("unknown ping reply"))?;

	let from = player.minecraft_uuid;
	let sent_at = Utc::now().timestamp_millis();
	for member in party_recipients(state, player).await? {
		send_to_owner(state, member, || ClientBoundPacket::PartyPingReply {
			id,
			owner,
			from,
			reply,
			sent_at,
		})
		.await;
	}

	Ok(())
}
