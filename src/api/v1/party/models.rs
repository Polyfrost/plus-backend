use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::{
	PartyInvitePolicy, PartyInviteStatus, PartyVoiceMode,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_LOCATION_KEY_CHARS: usize = 64;
const MAX_LOCATION_DISPLAY_CHARS: usize = 255;
const MAX_SCENE_ID_CHARS: usize = 64;
const MAX_SCENE_FALLBACK_CHARS: usize = 32;
const MAX_SCENE_CLOCK: u32 = 23999;
const MAX_MC_VERSION_CHARS: usize = 16;
const MAX_LOADER_CHARS: usize = 32;
const MAX_DESTINATION_ADDRESS_CHARS: usize = 255;
const MAX_DESTINATION_NAME_CHARS: usize = 64;
const MAX_DESTINATION_MODS: usize = 300;
const MAX_DESTINATION_MOD_CHARS: usize = 96;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Party {
	pub id: Uuid,
	pub leader: Uuid,
	pub created_at: DateTime<Utc>,
	pub settings: PartySettings,
	pub chat_group_id: Option<i32>,
	pub voice: PartyVoice,
	pub members: Vec<PartyMember>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PartySettings {
	pub voice_mode: PartyVoiceMode,
	pub invite_policy: PartyInvitePolicy,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PartyVoice {
	pub room_name: String,
	pub transport: VoiceTransport,
	pub rtc_available: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PartyMember {
	pub player: Uuid,
	pub joined_at: DateTime<Utc>,
	pub online: bool,
	pub eos_product_user_id: Option<String>,
	pub voice_hard_muted: bool,
	pub state: Option<MemberState>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PartyInvite {
	pub id: Uuid,
	pub party_id: Uuid,
	pub sender: Uuid,
	pub recipient: Uuid,
	pub status: PartyInviteStatus,
	pub created_at: DateTime<Utc>,
	pub expires_at: DateTime<Utc>,
	pub party_size: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VoiceToken {
	pub room_name: String,
	pub client_base_url: String,
	pub participant_token: String,
	pub issued_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VoicePass {
	pub pass: String,
	pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemberState {
	pub location: MemberLocation,
	#[serde(default)]
	pub voice: MemberVoice,
	#[serde(default)]
	pub svc_connected: bool,
	#[serde(default)]
	pub scene: Option<MemberScene>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemberScene {
	pub kind: SceneKind,
	pub id: String,
	#[serde(default)]
	pub fallback: String,
	pub clock: u32,
	#[serde(default)]
	pub mc_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SceneKind {
	Bundled,
	Capture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlayDestination {
	pub kind: PlayDestinationKind,
	#[serde(default)]
	pub address: Option<String>,
	pub name: String,
	#[serde(default)]
	pub mc_version: Option<String>,
	#[serde(default)]
	pub loader: Option<String>,
	#[serde(default)]
	pub mods: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlayDestinationKind {
	Server,
	World,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlayResponse {
	Joining,
	Cancelled,
	Busy,
	Incompatible,
	Joined,
	Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlayReason {
	Version,
	Unreachable,
	Mods,
	Full,
	Kicked,
	Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlayEndReason {
	Replaced,
	Expired,
	LeaderLeft,
	Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemberLocation {
	pub kind: LocationKind,
	#[serde(default)]
	pub key: Option<String>,
	#[serde(default)]
	pub display: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocationKind {
	Menu,
	Singleplayer,
	Server,
	HostedWorld,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MemberVoice {
	pub in_voice: bool,
	pub muted: bool,
	pub deafened: bool,
	pub mic_blocked: bool,
	pub capabilities: Vec<VoiceTransport>,
	pub transport: Option<VoiceTransport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VoiceTransport {
	Rtc,
	P2p,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PartyLeftReason {
	Left,
	Kicked,
	Disbanded,
	Expired,
}

impl MemberState {
	pub(in crate::api) fn validate(&self) -> Result<(), &'static str> {
		if self
			.location
			.key
			.as_ref()
			.is_some_and(|key| key.chars().count() > MAX_LOCATION_KEY_CHARS)
		{
			return Err("location.key is longer than 64 characters");
		}
		if self
			.location
			.display
			.as_ref()
			.is_some_and(|display| display.chars().count() > MAX_LOCATION_DISPLAY_CHARS)
		{
			return Err("location.display is longer than 255 characters");
		}

		let capabilities = &self.voice.capabilities;
		if capabilities
			.iter()
			.enumerate()
			.any(|(index, capability)| capabilities[..index].contains(capability))
		{
			return Err("voice.capabilities lists a capability twice");
		}

		if let Some(scene) = &self.scene {
			scene.validate()?;
		}

		Ok(())
	}

	pub(in crate::api) fn blocks_rtc(&self) -> bool {
		self.voice.in_voice && !self.voice.capabilities.contains(&VoiceTransport::Rtc)
	}
}

fn too_long(value: &str, max: usize) -> bool {
	value.chars().count() > max
}

impl MemberScene {
	fn validate(&self) -> Result<(), &'static str> {
		if too_long(&self.id, MAX_SCENE_ID_CHARS) {
			return Err("scene.id is longer than 64 characters");
		}
		if too_long(&self.fallback, MAX_SCENE_FALLBACK_CHARS) {
			return Err("scene.fallback is longer than 32 characters");
		}
		if self.clock > MAX_SCENE_CLOCK {
			return Err("scene.clock must be between 0 and 23999");
		}
		if too_long(&self.mc_version, MAX_MC_VERSION_CHARS) {
			return Err("scene.mc_version is longer than 16 characters");
		}
		Ok(())
	}
}

impl PlayDestination {
	pub(in crate::api) fn validate(&self) -> Result<(), &'static str> {
		match &self.address {
			Some(address) if too_long(address, MAX_DESTINATION_ADDRESS_CHARS) => {
				return Err("destination.address is longer than 255 characters");
			}
			None if self.kind == PlayDestinationKind::Server => {
				return Err("destination.address is required for a server");
			}
			_ => {}
		}
		if too_long(&self.name, MAX_DESTINATION_NAME_CHARS) {
			return Err("destination.name is longer than 64 characters");
		}
		if self
			.mc_version
			.as_deref()
			.is_some_and(|version| too_long(version, MAX_MC_VERSION_CHARS))
		{
			return Err("destination.mc_version is longer than 16 characters");
		}
		if self
			.loader
			.as_deref()
			.is_some_and(|loader| too_long(loader, MAX_LOADER_CHARS))
		{
			return Err("destination.loader is longer than 32 characters");
		}
		if self.mods.len() > MAX_DESTINATION_MODS {
			return Err("destination.mods has more than 300 entries");
		}
		if self
			.mods
			.iter()
			.any(|entry| too_long(entry, MAX_DESTINATION_MOD_CHARS))
		{
			return Err("destination.mods has an entry longer than 96 characters");
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn state(capabilities: Vec<VoiceTransport>, in_voice: bool) -> MemberState {
		MemberState {
			location: MemberLocation {
				kind: LocationKind::Menu,
				key: None,
				display: None,
			},
			voice: MemberVoice {
				in_voice,
				capabilities,
				..MemberVoice::default()
			},
			svc_connected: false,
			scene: None,
		}
	}

	#[test]
	fn rejects_duplicate_capabilities() {
		let duplicated = state(vec![VoiceTransport::Rtc, VoiceTransport::Rtc], true);
		assert!(duplicated.validate().is_err());
		assert!(state(vec![VoiceTransport::Rtc], true).validate().is_ok());
	}

	#[test]
	fn only_voice_members_without_rtc_block_it() {
		assert!(state(vec![VoiceTransport::P2p], true).blocks_rtc());
		assert!(!state(vec![VoiceTransport::P2p], false).blocks_rtc());
		assert!(!state(vec![VoiceTransport::Rtc], true).blocks_rtc());
	}

	#[test]
	fn server_destination_needs_an_address() {
		let mut destination = PlayDestination {
			kind: PlayDestinationKind::Server,
			address: None,
			name: "hub".to_string(),
			mc_version: None,
			loader: None,
			mods: Vec::new(),
		};
		assert!(destination.validate().is_err());

		destination.address = Some("play.example.com".to_string());
		assert!(destination.validate().is_ok());
	}
}
