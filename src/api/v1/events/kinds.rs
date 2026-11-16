use schemars::JsonSchema;
use sea_orm::prelude::DateTimeWithTimeZone;
use serde::{Deserialize, Serialize};

use super::{daily::DailyRewards, requirements::Requirement, rewards::Reward};

/// Every kind of event. A new kind is a variant here and a module that turns
/// its settings into [`Slot`]s; claiming, listing and validation are shared.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum EventKind {
	DailyRewards(DailyRewards),
}

/// One reward a player can claim once, while it is open.
pub(super) struct Slot<'a> {
	/// Stable across edits: claims are recorded against it.
	pub(super) key: String,
	pub(super) opens_at: DateTimeWithTimeZone,
	pub(super) closes_at: DateTimeWithTimeZone,
	pub(super) reward: &'a Reward,
	/// All of them must be met to claim.
	pub(super) requirements: &'a [Requirement],
}

impl EventKind {
	pub(super) fn slots(&self) -> Vec<Slot<'_>> {
		match self {
			Self::DailyRewards(daily) => daily.slots(),
		}
	}
}
