use std::collections::HashSet;

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
	now: DateTime<Utc>,
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
			ended_at: row.ended_at.map_or(now, Into::into),
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
			// Sessions already running when the hour began.
			let mut concurrent = 0i32;
			let mut events: Vec<(DateTime<Utc>, i32)> = Vec::new();

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
					events.push((span.started_at, 1));
				} else {
					concurrent += 1;
				}

				if span.ended_at < hour_end {
					events.push((span.ended_at, -1));
				}
			}

			events.sort_unstable_by_key(|&(at, delta)| (at, -delta));

			let mut peak_concurrent = concurrent;
			for (_, delta) in events {
				concurrent += delta;
				peak_concurrent = peak_concurrent.max(concurrent);
			}

			Some(HourBucket {
				hour_start,
				playtime_seconds,
				active_players: players.len().try_into().unwrap_or(i32::MAX),
				sessions_started,
				peak_concurrent,
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
