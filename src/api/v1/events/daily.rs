use chrono::{Duration, NaiveDate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{kinds::Slot, requirements::Requirement, rewards::Reward};
use crate::utils::time::start_of_day;

/// One reward per UTC day, each claimable for `claim_window_hours` from the
/// start of its day. Slots are keyed by the day, e.g. `2026-12-01`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub(super) struct DailyRewards {
	claim_window_hours: u16,
	rewards: Vec<DayReward>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct DayReward {
	day: NaiveDate,
	reward: Reward,
	#[serde(default)]
	requirements: Vec<Requirement>,
}

impl DailyRewards {
	pub(super) fn slots(&self) -> Vec<Slot<'_>> {
		self.rewards
			.iter()
			.map(|DayReward { day, reward, requirements }| {
				let opens_at = start_of_day(*day);
				Slot {
					key: day.to_string(),
					opens_at,
					closes_at: opens_at + Duration::hours(self.claim_window_hours.into()),
					reward,
					requirements,
				}
			})
			.collect()
	}
}
