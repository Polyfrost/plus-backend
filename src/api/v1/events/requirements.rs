use chrono::TimeDelta;
use entities::{
	game_sessions, group_messages, linked_account, play_session, prelude::*, relationships,
	sea_orm_active_enums::LinkProvider,
};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter,
	prelude::DateTimeWithTimeZone,
};
use serde::{Deserialize, Serialize};

/// Something a player has to do before claiming: reach `target` of what
/// `type` measures. Activity counts from when the reward opens.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub(super) struct Requirement {
	#[serde(flatten)]
	measure: Measure,
	target: u32,
}

/// What a requirement counts. New kinds of requirement are new variants.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Measure {
	/// Seconds in game; the launcher does not count.
	Playtime,
	/// Messages sent in DMs, groups or parties.
	ChatMessages,
	/// Friends the player has, best friends included, however long ago they
	/// were added.
	Friends,
	/// Seconds spent hosting a world.
	HostWorld,
	/// 1 once an account on `provider` is linked, so its target is 1.
	LinkedAccount { provider: LinkProvider },
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Progress {
	#[serde(flatten)]
	requirement: Requirement,
	/// Where the player is, in the same unit as `target`. Null unless the
	/// reward is open and unclaimed.
	current: Option<u32>,
	pub(super) met: bool,
}

impl Requirement {
	pub(super) fn validate(&self) -> Result<(), &'static str> {
		match self.measure {
			Measure::LinkedAccount { .. } if self.target != 1 => {
				Err("A linked account requirement's target is 1")
			}
			_ if self.target == 0 => Err("A requirement needs a target above zero"),
			_ => Ok(()),
		}
	}

	async fn current(
		&self,
		db: &impl ConnectionTrait,
		player_id: i32,
		from: DateTimeWithTimeZone,
		to: DateTimeWithTimeZone,
	) -> Result<u32, DbErr> {
		match &self.measure {
			Measure::Playtime => {
				let sessions = PlaySession::find()
					.filter(play_session::Column::PlayerId.eq(player_id))
					.filter(play_session::Column::StartedAt.lt(to))
					.filter(
						Condition::any()
							.add(play_session::Column::EndedAt.gt(from))
							.add(play_session::Column::LastHeartbeatAt.gt(from)),
					)
					.all(db)
					.await?;
				Ok(seconds(
					sessions
						.into_iter()
						.map(|session| {
							(session.started_at, session.ended_at.unwrap_or(session.last_heartbeat_at))
						})
						.collect(),
					from,
					to,
				))
			}
			Measure::HostWorld => {
				let sessions = GameSessions::find()
					.filter(game_sessions::Column::HostId.eq(player_id))
					.filter(game_sessions::Column::CreatedAt.lt(to))
					.filter(game_sessions::Column::ExpiresAt.gt(from))
					.all(db)
					.await?;
				Ok(seconds(
					sessions
						.into_iter()
						.map(|session| (session.created_at, session.expires_at))
						.collect(),
					from,
					to,
				))
			}
			Measure::ChatMessages => Ok(count(
				GroupMessages::find()
					.filter(group_messages::Column::SenderId.eq(player_id))
					.filter(group_messages::Column::SentAt.gte(from))
					.filter(group_messages::Column::SentAt.lt(to))
					.filter(group_messages::Column::DeletedAt.is_null())
					.filter(group_messages::Column::SentById.is_null())
					.count(db)
					.await?,
			)),
			Measure::Friends => Ok(count(
				Relationships::find()
					.filter(
						Condition::any()
							.add(relationships::Column::UserAId.eq(player_id))
							.add(relationships::Column::UserBId.eq(player_id)),
					)
					.count(db)
					.await?,
			)),
			Measure::LinkedAccount { provider } => Ok(count(
				LinkedAccount::find()
					.filter(linked_account::Column::UserId.eq(player_id))
					.filter(linked_account::Column::Provider.eq(provider.clone()))
					.count(db)
					.await?,
			)),
		}
	}
}

/// Each requirement with the player's progress, measured between `from` and
/// `to` when a window is given.
pub(super) async fn progress(
	db: &impl ConnectionTrait,
	player_id: i32,
	requirements: &[Requirement],
	window: Option<(DateTimeWithTimeZone, DateTimeWithTimeZone)>,
) -> Result<Vec<Progress>, DbErr> {
	let mut progress = Vec::with_capacity(requirements.len());
	for requirement in requirements {
		let current = match window {
			Some((from, to)) => Some(requirement.current(db, player_id, from, to).await?),
			None => None,
		};
		progress.push(Progress {
			requirement: requirement.clone(),
			current,
			met: current.is_some_and(|current| current >= requirement.target),
		});
	}

	Ok(progress)
}

fn count(rows: u64) -> u32 {
	u32::try_from(rows).unwrap_or(u32::MAX)
}

/// Seconds inside `from..to` that any span covers, overlaps counted once.
fn seconds(
	mut spans: Vec<(DateTimeWithTimeZone, DateTimeWithTimeZone)>,
	from: DateTimeWithTimeZone,
	to: DateTimeWithTimeZone,
) -> u32 {
	spans.sort_unstable();

	let mut covered = TimeDelta::zero();
	let mut reached = from;
	for (start, end) in spans {
		let start = start.max(reached);
		let end = end.min(to);
		if end > start {
			covered += end - start;
			reached = end;
		}
	}

	u32::try_from(covered.num_seconds()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
	use chrono::{NaiveDate, TimeDelta};

	use super::seconds;
	use crate::utils::time::start_of_day;

	#[test]
	fn overlapping_spans_count_once_and_are_clipped_to_the_window() {
		let from = start_of_day(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap());
		let at = |minute| from + TimeDelta::minutes(minute);

		assert_eq!(seconds(vec![], from, at(60)), 0);
		assert_eq!(seconds(vec![(at(-30), at(10))], from, at(60)), 600, "started before");
		assert_eq!(seconds(vec![(at(50), at(90))], from, at(60)), 600, "still going");
		assert_eq!(
			seconds(vec![(at(20), at(40)), (at(0), at(30)), (at(25), at(26))], from, at(60)),
			2400,
			"overlaps",
		);
		assert_eq!(seconds(vec![(at(0), at(5)), (at(10), at(15))], from, at(60)), 600, "gaps");
	}
}
