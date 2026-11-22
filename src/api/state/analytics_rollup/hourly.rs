use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike as _, NaiveDate, TimeDelta, Timelike as _, Utc};
use entities::{
	analytics_hourly, analytics_session_length_daily, play_session, prelude::*,
};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	QueryFilter as _, prelude::DateTimeWithTimeZone, sea_query::Condition,
};

use super::day_bounds;

const SESSION_LOOKBACK_HOURS: i64 = 48;

#[derive(Debug, Clone, Copy)]
pub(super) struct SessionSpan {
	player_id: i32,
	started_at: DateTime<Utc>,
	ended_at: DateTime<Utc>,
}

#[derive(Debug)]
pub(super) struct HourBucket {
	hour_start: DateTime<Utc>,
	playtime_seconds: i64,
	active_players: i32,
	sessions_started: i32,
	peak_concurrent: i32,
}

pub(super) async fn session_spans_for_day(
	txn: &DatabaseTransaction,
	day: NaiveDate,
) -> Result<Vec<SessionSpan>, DbErr> {
	let (day_start, day_end) = day_bounds(day);
	let lookback = day_start - TimeDelta::hours(SESSION_LOOKBACK_HOURS);

	let rows = PlaySession::find()
		.filter(play_session::Column::StartedAt.gte(lookback))
		.filter(play_session::Column::StartedAt.lt(day_end))
		.filter(
			Condition::any()
				.add(play_session::Column::EndedAt.is_null())
				.add(play_session::Column::EndedAt.gte(day_start)),
		)
		.all(txn)
		.await?;

	Ok(rows
		.into_iter()
		.map(|row| SessionSpan {
			player_id: row.player_id,
			started_at: row.started_at.into(),
			// An open session is only known to run until its last heartbeat; one
			// whose process died would otherwise grow until the next reap.
			ended_at: row.ended_at.unwrap_or(row.last_heartbeat_at).into(),
		})
		.collect())
}

pub(super) fn bucket_sessions_into_hours(
	day: NaiveDate,
	spans: &[SessionSpan],
) -> Vec<HourBucket> {
	(0..24)
		.filter_map(|hour| {
			let hour_start = day.and_hms_opt(hour, 0, 0)?.and_utc();
			let hour_end = hour_start + TimeDelta::hours(1);

			let mut playtime_seconds = 0i64;
			let mut players = HashSet::new();
			let mut sessions_started = 0i32;
			// Open sessions per player, so two clients at once count one player.
			let mut open: HashMap<i32, i32> = HashMap::new();
			let mut events: Vec<(DateTime<Utc>, i32, i32)> = Vec::new();

			for span in spans {
				let overlap_start = span.started_at.max(hour_start);
				let overlap_end = span.ended_at.min(hour_end);
				if overlap_end <= overlap_start {
					continue;
				}

				playtime_seconds += (overlap_end - overlap_start).num_seconds();
				players.insert(span.player_id);

				if span.started_at >= hour_start {
					sessions_started += 1;
					events.push((span.started_at, 1, span.player_id));
				} else {
					*open.entry(span.player_id).or_default() += 1;
				}

				if span.ended_at < hour_end {
					events.push((span.ended_at, -1, span.player_id));
				}
			}

			// Ends before starts at the same instant, so a reconnect is not
			// counted as two players.
			events.sort_unstable_by_key(|&(at, delta, _)| (at, delta));

			let mut concurrent = open.len();
			let mut peak_concurrent = concurrent;
			for (_, delta, player_id) in events {
				let sessions = open.entry(player_id).or_default();
				let was_online = *sessions > 0;
				*sessions += delta;
				match (was_online, *sessions > 0) {
					(false, true) => concurrent += 1,
					(true, false) => concurrent -= 1,
					_ => {}
				}
				peak_concurrent = peak_concurrent.max(concurrent);
			}

			Some(HourBucket {
				hour_start,
				playtime_seconds,
				active_players: players.len().try_into().unwrap_or(i32::MAX),
				sessions_started,
				peak_concurrent: peak_concurrent.try_into().unwrap_or(i32::MAX),
			})
		})
		.collect()
}

pub(super) fn hourly_rows(
	day: NaiveDate,
	buckets: Vec<HourBucket>,
	computed_at: DateTimeWithTimeZone,
) -> Vec<analytics_hourly::ActiveModel> {
	let dow = i16::try_from(day.weekday().number_from_monday()).unwrap_or(1);

	buckets
		.into_iter()
		.map(|bucket| analytics_hourly::ActiveModel {
			hour_start: ActiveValue::Set(bucket.hour_start.into()),
			dow: ActiveValue::Set(dow),
			hour_of_day: ActiveValue::Set(
				i16::try_from(bucket.hour_start.hour()).unwrap_or(0),
			),
			playtime_seconds: ActiveValue::Set(bucket.playtime_seconds),
			active_players: ActiveValue::Set(bucket.active_players),
			sessions_started: ActiveValue::Set(bucket.sessions_started),
			peak_concurrent: ActiveValue::Set(bucket.peak_concurrent),
			computed_at: ActiveValue::Set(computed_at),
		})
		.collect()
}

/// Inclusive lower bounds; the last bucket is open ended.
pub(in crate::api) const SESSION_LENGTH_BUCKETS: [i64; 8] =
	[0, 60, 300, 900, 1800, 3600, 7200, 14400];

fn bucket_index(seconds: i64) -> usize {
	SESSION_LENGTH_BUCKETS
		.iter()
		.rposition(|&lower| seconds >= lower)
		.unwrap_or(0)
}

pub(super) fn sessions_started(day: NaiveDate, spans: &[SessionSpan]) -> i32 {
	let (day_start, day_end) = day_bounds(day);
	let started = spans
		.iter()
		.filter(|span| span.started_at >= day_start && span.started_at < day_end)
		.count();

	started.try_into().unwrap_or(i32::MAX)
}

/// Counts only sessions that *started* on `day`, so one spanning midnight lands
/// in exactly one day's histogram.
pub(super) fn session_length_rows(
	day: NaiveDate,
	spans: &[SessionSpan],
	computed_at: DateTimeWithTimeZone,
) -> Vec<analytics_session_length_daily::ActiveModel> {
	let (day_start, day_end) = day_bounds(day);
	let mut counts = [0i32; SESSION_LENGTH_BUCKETS.len()];

	for span in spans {
		if span.started_at < day_start || span.started_at >= day_end {
			continue;
		}

		let seconds = (span.ended_at - span.started_at).num_seconds().max(0);
		counts[bucket_index(seconds)] += 1;
	}

	// Zeros included: an open session lengthens between runs and can change
	// bucket, leaving a stale count behind in the one it left.
	counts
		.into_iter()
		.enumerate()
		.map(
			|(bucket, sessions)| analytics_session_length_daily::ActiveModel {
				day: ActiveValue::Set(day),
				bucket: ActiveValue::Set(i16::try_from(bucket).unwrap_or(0)),
				sessions: ActiveValue::Set(sessions),
				computed_at: ActiveValue::Set(computed_at),
			},
		)
		.collect()
}

#[cfg(test)]
mod tests {
	use chrono::{DateTime, NaiveDate, TimeDelta, Utc};

	use super::{SessionSpan, bucket_sessions_into_hours};

	fn span(player_id: i32, from_minute: i64, to_minute: i64) -> SessionSpan {
		let midnight = DateTime::<Utc>::from_naive_utc_and_offset(
			NaiveDate::from_ymd_opt(2026, 11, 22)
				.and_then(|day| day.and_hms_opt(0, 0, 0))
				.expect("valid time"),
			Utc,
		);
		SessionSpan {
			player_id,
			started_at: midnight + TimeDelta::minutes(from_minute),
			ended_at: midnight + TimeDelta::minutes(to_minute),
		}
	}

	#[test]
	fn peak_counts_players_not_sessions_or_reconnects() {
		let day = NaiveDate::from_ymd_opt(2026, 11, 22).expect("valid date");
		let spans = [
			// Reconnects at minute 20.
			span(1, 0, 20),
			span(1, 20, 50),
			// Two clients at once.
			span(2, 10, 40),
			span(2, 15, 30),
			// Already online when the hour began.
			span(3, -30, 5),
		];

		let first_hour = &bucket_sessions_into_hours(day, &spans)[0];

		assert_eq!(first_hour.peak_concurrent, 2);
		assert_eq!(first_hour.active_players, 3);
		assert_eq!(first_hour.sessions_started, 4);
	}
}
