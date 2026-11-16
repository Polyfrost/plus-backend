//! Limits staff can change from the dashboard without a restart, kept in a
//! JSON file and cached in memory. Edits to the file apply on restart.

use std::{
	fs,
	io::{self, ErrorKind},
	path::Path,
	sync::{Arc, PoisonError, RwLock},
};

use aide::OperationIo;
use axum::{http::StatusCode, response::IntoResponse};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::v1::MAX_UNCOMPRESSED_BYTES;

const DAY_SECS: u64 = 24 * 60 * 60;
/// Keeps every duration far from chrono's overflow panics.
const MAX_DURATION_SECS: u64 = 10 * 365 * DAY_SECS;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub(crate) struct Settings {
	pub(crate) cluster_sharing: ClusterSharing,
	pub(crate) chat: Chat,
	pub(crate) outfits: Outfits,
}

/// What players may sync and share. Served to every player.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub(crate) struct ClusterSharing {
	/// The largest a single instance may be.
	pub(crate) max_payload_bytes: u64,
	/// Everything a player syncs and shares, together.
	pub(crate) max_account_bytes: i64,
	pub(crate) max_synced_instances: u64,
	pub(crate) max_public_shares: u64,
	pub(crate) max_collaborators: u64,
	/// How long a public share lasts after it was last downloaded or updated.
	pub(crate) public_share_ttl_secs: u64,
	pub(crate) write_window_secs: u64,
	pub(crate) writes_per_window: u32,
	/// Writes over this size also count against `large_writes_per_window`.
	pub(crate) large_write_bytes: i64,
	pub(crate) large_writes_per_window: u32,
	/// Deleted instances remembered per player, so other devices learn of it.
	pub(crate) max_tombstones: u64,
	pub(crate) tombstone_retention_secs: u64,
}

impl Default for ClusterSharing {
	fn default() -> Self {
		Self {
			max_payload_bytes: 50 * 1024 * 1024,
			max_account_bytes: 12 * 1024 * 1024 * 1024 / 10,
			max_synced_instances: 2,
			max_public_shares: 10,
			max_collaborators: 10,
			public_share_ttl_secs: 90 * DAY_SECS,
			write_window_secs: 60 * 60,
			writes_per_window: 240,
			large_write_bytes: 16 * 1024 * 1024,
			large_writes_per_window: 24,
			max_tombstones: 500,
			tombstone_retention_secs: 90 * DAY_SECS,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub(crate) struct Chat {
	/// For group, DM and party chat messages.
	pub(crate) max_message_length: usize,
	/// Between one player's messages to a Special Chat target.
	pub(crate) special_chat_cooldown_secs: u64,
}

impl Default for Chat {
	fn default() -> Self {
		Self {
			max_message_length: 4000,
			special_chat_cooldown_secs: 3 * DAY_SECS,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub(crate) struct Outfits {
	pub(crate) max_outfits: u64,
	pub(crate) max_skins: u64,
}

impl Default for Outfits {
	fn default() -> Self {
		Self {
			max_outfits: 128,
			max_skins: 128,
		}
	}
}

impl Settings {
	fn validate(&self) -> Result<(), SettingsError> {
		let sharing = &self.cluster_sharing;
		let durations = [
			sharing.public_share_ttl_secs,
			sharing.write_window_secs,
			sharing.tombstone_retention_secs,
			self.chat.special_chat_cooldown_secs,
		];
		let rules = [
			(
				(1..=MAX_UNCOMPRESSED_BYTES).contains(&sharing.max_payload_bytes),
				"cluster_sharing.max_payload_bytes must be between 1 byte and the 256 MiB uncompressed archive limit",
			),
			(
				sharing.max_account_bytes > 0 && sharing.large_write_bytes >= 0,
				"Byte limits must not be negative",
			),
			(
				sharing.public_share_ttl_secs > 0 && sharing.write_window_secs > 0,
				"cluster_sharing durations must be positive",
			),
			(
				sharing.writes_per_window > 0 && sharing.large_writes_per_window > 0,
				"cluster_sharing write limits must be positive",
			),
			(
				self.chat.max_message_length > 0,
				"chat.max_message_length must be positive",
			),
			(self.outfits.max_outfits > 0, "outfits.max_outfits must be positive"),
			(
				durations.iter().all(|&secs| secs <= MAX_DURATION_SECS),
				"Durations must be at most 10 years",
			),
		];
		match rules.into_iter().find(|(valid, _)| !valid) {
			Some((_, message)) => Err(SettingsError::Invalid(message.to_owned())),
			None => Ok(()),
		}
	}
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub(crate) enum SettingsError {
	#[error("{0}")]
	Invalid(String),
	#[error("Unable to save settings: {0}")]
	Io(#[from] io::Error),
}

impl IntoResponse for SettingsError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::Invalid(_) => StatusCode::BAD_REQUEST,
				Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

/// Assumes a single backend instance: another one would keep its own copy.
#[derive(Debug, Clone)]
pub(crate) struct SharedSettings {
	path: Arc<Path>,
	current: Arc<RwLock<Arc<Settings>>>,
}

impl SharedSettings {
	/// The defaults until the file exists.
	pub(crate) fn load(path: &Path) -> io::Result<Self> {
		let settings = match fs::read(path) {
			Ok(bytes) => {
				let settings: Settings = serde_json::from_slice(&bytes)?;
				settings.validate().map_err(io::Error::other)?;
				settings
			}
			Err(error) if error.kind() == ErrorKind::NotFound => Settings::default(),
			Err(error) => return Err(error),
		};
		Ok(Self {
			path: path.into(),
			current: Arc::new(RwLock::new(Arc::new(settings))),
		})
	}

	pub(crate) fn get(&self) -> Arc<Settings> {
		self.current
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}

	/// Applies `patch` as a JSON merge patch, so only the fields sent change.
	/// Blocks readers while the small file is written, which keeps writes in order.
	pub(crate) fn update(&self, patch: Value) -> Result<Arc<Settings>, SettingsError> {
		let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);

		let settings = patched(&current, patch)?;
		// Renamed into place, so a crash mid-write never leaves half a file.
		let staged = self.path.with_extension("json.tmp");
		fs::write(&staged, serde_json::to_vec_pretty(&settings).map_err(io::Error::from)?)?;
		fs::rename(&staged, &self.path)?;

		*current = Arc::new(settings);
		Ok(current.clone())
	}
}

fn patched(current: &Settings, patch: Value) -> Result<Settings, SettingsError> {
	let mut value = serde_json::to_value(current)
		.map_err(|error| SettingsError::Invalid(error.to_string()))?;
	merge(&mut value, patch);
	let settings: Settings = serde_json::from_value(value)
		.map_err(|error| SettingsError::Invalid(error.to_string()))?;
	settings.validate()?;
	Ok(settings)
}

fn merge(target: &mut Value, patch: Value) {
	match (target, patch) {
		(Value::Object(target), Value::Object(patch)) => {
			for (key, value) in patch {
				merge(target.entry(key).or_insert(Value::Null), value);
			}
		}
		(target, patch) => *target = patch,
	}
}

#[cfg(test)]
mod tests {
	use std::{env, fs};

	use serde_json::json;
	use uuid::Uuid;

	use super::{Settings, SharedSettings, patched};

	#[test]
	fn changes_survive_a_reload() {
		let path = env::temp_dir().join(format!("settings-{}.json", Uuid::now_v7()));
		let settings = SharedSettings::load(&path).expect("defaults");
		assert_eq!(settings.get().outfits.max_outfits, 128);

		settings
			.update(json!({ "outfits": { "max_outfits": 7 } }))
			.expect("saved");
		let reloaded = SharedSettings::load(&path).expect("reloaded");
		let _ = fs::remove_file(&path);
		assert_eq!(reloaded.get().outfits.max_outfits, 7);
		assert_eq!(reloaded.get().outfits.max_skins, 128);
	}

	#[test]
	fn patch_changes_only_the_fields_sent() {
		let settings = patched(
			&Settings::default(),
			json!({ "cluster_sharing": { "max_public_shares": 3 } }),
		)
		.expect("valid patch");
		assert_eq!(settings.cluster_sharing.max_public_shares, 3);
		assert_eq!(settings.cluster_sharing.max_collaborators, 10);
		assert_eq!(settings.outfits.max_skins, 128);
	}

	#[test]
	fn invalid_patches_are_refused() {
		let defaults = Settings::default();
		for patch in [
			json!({ "cluster_sharing": { "max_payload_bytes": 0 } }),
			json!({ "cluster_sharing": { "max_payload_bytes": 1u64 << 40 } }),
			json!({ "cluster_sharing": { "write_window_secs": 0 } }),
			json!({ "cluster_sharing": { "max_account_bytes": -1 } }),
			json!({ "chat": { "max_message_length": 0 } }),
			json!({ "chat": { "special_chat_cooldown_secs": u64::MAX } }),
			json!({ "outfits": { "max_outfits": "lots" } }),
		] {
			assert!(patched(&defaults, patch.clone()).is_err(), "{patch}");
		}
	}
}
