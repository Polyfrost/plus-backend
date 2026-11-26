
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	time::Duration,
};

use chrono::Utc;
use entities::{perf_pack, perf_regression, perf_report, prelude::*};
use sea_orm::{
	ColumnTrait as _, ConnectionTrait as _, DatabaseConnection, DbBackend, DbErr, EntityTrait, FromQueryResult,
	QueryFilter as _, Set, Statement,
	prelude::DateTimeWithTimeZone,
	sea_query::{Expr, OnConflict},
};
use tracing::{info, warn};

const RUN_INTERVAL: Duration = Duration::from_secs(60 * 60);
const WINDOW_DAYS: i32 = 30;
pub(in crate::api) const RETENTION_DAYS: i32 = 90;
const MIN_PAIRS: usize = 20;
const PACK_CHUNK: usize = 10_000;
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ALERT_LINES: usize = 15;
const MAX_ALERT_LEN: usize = 1900;

const METRICS: [(&str, f64); 3] = [("median_fps", 0.95), ("low1_fps", 0.92), ("low01_fps", 0.90)];

pub(super) fn spawn_perf_regression(database: DatabaseConnection, webhook: Option<String>) {
	tokio::spawn(async move {
		let http = reqwest::Client::new();
		loop {
			if let Err(error) = run(&database, &http, webhook.as_deref()).await {
				warn!("Unable to check for performance regressions: {error}");
			}
			tokio::time::sleep(RUN_INTERVAL).await;
		}
	});
}

#[derive(Debug, FromQueryResult)]
struct Cohort {
	minecraft_version: String,
	loader: String,
}

#[derive(Debug, Clone, FromQueryResult)]
struct PlayerPack {
	player_id: i32,
	pack_hash: String,
	gpu: String,
	cpu: String,
	render_distance: i32,
	shaders: bool,
	first_seen: DateTimeWithTimeZone,
	median_fps: f64,
	low1_fps: f64,
	low01_fps: f64,
}

impl PlayerPack {
	fn metrics(&self) -> [f64; 3] {
		[self.median_fps, self.low1_fps, self.low01_fps]
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Change {
	mod_id: String,
	from: String,
	to: String,
}

#[derive(Debug, PartialEq)]
struct Transition {
	player_id: i32,
	changes: Vec<Change>,
	ratios: [f64; 3],
}

#[derive(Debug, Default)]
struct Evidence {
	all: [Vec<f64>; 3],
	solo: [Vec<f64>; 3],
}

async fn run(
	database: &DatabaseConnection,
	http: &reqwest::Client,
	webhook: Option<&str>,
) -> Result<(), DbErr> {
	PerfReport::delete_many()
		.filter(
			perf_report::Column::ReportedAt
				.lt(Utc::now() - chrono::Duration::days(RETENTION_DAYS.into())),
		)
		.exec(database)
		.await?;
	database
		.execute_unprepared(
			"DELETE FROM perf_pack p \
			 WHERE first_seen_at < now() - interval '1 day' \
			   AND NOT EXISTS (SELECT 1 FROM perf_report r WHERE r.pack_hash = p.pack_hash)",
		)
		.await?;
	PerfRegression::delete_many()
		.filter(
			perf_regression::Column::UpdatedAt
				.lt(Utc::now() - chrono::Duration::days(WINDOW_DAYS.into())),
		)
		.exec(database)
		.await?;

	super::perf_daily::rollup(database).await?;

	let cohorts = Cohort::find_by_statement(Statement::from_sql_and_values(
		DbBackend::Postgres,
		"SELECT DISTINCT minecraft_version, loader FROM perf_report \
		 WHERE reported_at > now() - make_interval(days => $1)",
		[WINDOW_DAYS.into()],
	))
	.all(database)
	.await?;

	for cohort in cohorts {
		check_cohort(database, &cohort).await?;
	}

	if let Some(webhook) = webhook {
		notify(database, http, webhook).await?;
	}
	Ok(())
}

async fn check_cohort(database: &DatabaseConnection, cohort: &Cohort) -> Result<(), DbErr> {
	let rows = PlayerPack::find_by_statement(Statement::from_sql_and_values(
		DbBackend::Postgres,
		"SELECT player_id, pack_hash, \
		        coalesce(gpu, '') AS gpu, coalesce(cpu, '') AS cpu, \
		        coalesce(render_distance, -1) AS render_distance, \
		        coalesce(shaders, false) AS shaders, \
		        min(reported_at) AS first_seen, \
		        percentile_cont(0.5) WITHIN GROUP (ORDER BY median_fps) AS median_fps, \
		        percentile_cont(0.5) WITHIN GROUP (ORDER BY low1_fps) AS low1_fps, \
		        percentile_cont(0.5) WITHIN GROUP (ORDER BY low01_fps) AS low01_fps \
		 FROM perf_report \
		 WHERE minecraft_version = $1 AND loader = $2 \
		   AND reported_at > now() - make_interval(days => $3) \
		   AND vsync IS NOT TRUE \
		   AND (max_fps IS NULL OR median_fps < max_fps * 0.9) \
		 GROUP BY 1, 2, 3, 4, 5, 6",
		[
			cohort.minecraft_version.clone().into(),
			cohort.loader.clone().into(),
			WINDOW_DAYS.into(),
		],
	))
	.all(database)
	.await?;

	let steps = consecutive(&rows);
	let needed: Vec<String> = steps
		.iter()
		.flat_map(|(a, b)| [a.pack_hash.clone(), b.pack_hash.clone()])
		.collect::<HashSet<_>>()
		.into_iter()
		.collect();
	let packs = load_packs(database, &needed).await?;
	let transitions = steps
		.into_iter()
		.filter_map(|(a, b)| transition(a, b, &packs))
		.collect::<Vec<_>>();

	for (change, evidence) in attribute(&transitions) {
		for (index, (metric, threshold)) in METRICS.into_iter().enumerate() {
			let Some((ratio, pairs)) = median(&evidence.all[index]) else {
				continue;
			};
			if pairs < MIN_PAIRS {
				continue;
			}
			let row = perf_regression::ActiveModel {
				minecraft_version: Set(cohort.minecraft_version.clone()),
				loader: Set(cohort.loader.clone()),
				mod_id: Set(change.mod_id.clone()),
				from_version: Set(change.from.clone()),
				to_version: Set(change.to.clone()),
				metric: Set(metric.to_owned()),
				pairs: Set(count(pairs)),
				ratio: Set(ratio),
				solo_pairs: Set(count(evidence.solo[index].len())),
				solo_ratio: Set(median(&evidence.solo[index]).map(|(r, _)| r)),
				..Default::default()
			};
			if ratio < threshold {
				record(database, row).await?;
			} else {
				PerfRegression::delete_by_id((
					cohort.minecraft_version.clone(),
					cohort.loader.clone(),
					change.mod_id.clone(),
					change.from.clone(),
					change.to.clone(),
					metric.to_owned(),
				))
				.exec(database)
				.await?;
			}
		}
	}
	Ok(())
}

fn count(n: usize) -> i32 {
	i32::try_from(n).unwrap_or(i32::MAX)
}

fn consecutive(rows: &[PlayerPack]) -> Vec<(&PlayerPack, &PlayerPack)> {
	let mut setups: HashMap<_, Vec<&PlayerPack>> = HashMap::new();
	for row in rows {
		setups
			.entry((row.player_id, &row.gpu, &row.cpu, row.render_distance, row.shaders))
			.or_default()
			.push(row);
	}
	setups
		.into_values()
		.flat_map(|mut packs| {
			packs.sort_by_key(|p| p.first_seen);
			packs.windows(2).map(|w| (w[0], w[1])).collect::<Vec<_>>()
		})
		.collect()
}

fn transition(
	before: &PlayerPack,
	after: &PlayerPack,
	packs: &HashMap<String, BTreeMap<String, String>>,
) -> Option<Transition> {
	let changes = diff(packs.get(&before.pack_hash)?, packs.get(&after.pack_hash)?);
	if changes.is_empty() {
		return None;
	}
	let (b, a) = (before.metrics(), after.metrics());
	if b.iter().any(|v| *v <= 0.0) {
		return None;
	}
	Some(Transition {
		player_id: before.player_id,
		changes,
		ratios: [a[0] / b[0], a[1] / b[1], a[2] / b[2]],
	})
}

fn diff(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<Change> {
	let ids: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
	ids.into_iter()
		.filter_map(|id| {
			let from = before.get(id).cloned().unwrap_or_default();
			let to = after.get(id).cloned().unwrap_or_default();
			(from != to).then(|| Change {
				mod_id: id.clone(),
				from,
				to,
			})
		})
		.collect()
}

fn attribute(transitions: &[Transition]) -> HashMap<Change, Evidence> {
	let mut per_player: HashMap<(&Change, i32), Evidence> = HashMap::new();
	for transition in transitions {
		let solo = transition.changes.len() == 1;
		for change in &transition.changes {
			let entry = per_player.entry((change, transition.player_id)).or_default();
			for (index, ratio) in transition.ratios.iter().enumerate() {
				entry.all[index].push(*ratio);
				if solo {
					entry.solo[index].push(*ratio);
				}
			}
		}
	}

	let mut evidence: HashMap<Change, Evidence> = HashMap::new();
	for ((change, _), player) in per_player {
		let entry = evidence.entry(change.clone()).or_default();
		for index in 0..METRICS.len() {
			if let Some((ratio, _)) = median(&player.all[index]) {
				entry.all[index].push(ratio);
			}
			if let Some((ratio, _)) = median(&player.solo[index]) {
				entry.solo[index].push(ratio);
			}
		}
	}
	evidence
}

fn median(values: &[f64]) -> Option<(f64, usize)> {
	let mut values: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
	if values.is_empty() {
		return None;
	}
	values.sort_by(f64::total_cmp);
	let mid = values.len() / 2;
	let median = if values.len().is_multiple_of(2) {
		(values[mid - 1] + values[mid]) / 2.0
	} else {
		values[mid]
	};
	Some((median, values.len()))
}

async fn load_packs(
	database: &DatabaseConnection,
	hashes: &[String],
) -> Result<HashMap<String, BTreeMap<String, String>>, DbErr> {
	let mut packs = HashMap::new();
	for chunk in hashes.chunks(PACK_CHUNK) {
		for pack in PerfPack::find()
			.filter(perf_pack::Column::PackHash.is_in(chunk.iter().cloned()))
			.all(database)
			.await?
		{
			if let Ok(mods) = serde_json::from_value(pack.mods) {
				packs.insert(pack.pack_hash, mods);
			}
		}
	}
	Ok(packs)
}

async fn record(database: &DatabaseConnection, row: perf_regression::ActiveModel) -> Result<(), DbErr> {
	PerfRegression::insert(row)
		.on_conflict(
			OnConflict::columns([
				perf_regression::Column::MinecraftVersion,
				perf_regression::Column::Loader,
				perf_regression::Column::ModId,
				perf_regression::Column::FromVersion,
				perf_regression::Column::ToVersion,
				perf_regression::Column::Metric,
			])
			.update_columns([
				perf_regression::Column::Pairs,
				perf_regression::Column::Ratio,
				perf_regression::Column::SoloPairs,
				perf_regression::Column::SoloRatio,
			])
			.value(perf_regression::Column::UpdatedAt, Expr::current_timestamp())
			.to_owned(),
		)
		.exec_without_returning(database)
		.await?;
	Ok(())
}

fn describe(r: &perf_regression::Model) -> String {
	let change = match (r.from_version.is_empty(), r.to_version.is_empty()) {
		(true, _) => format!("`{}` added ({})", r.mod_id, r.to_version),
		(_, true) => format!("`{}` removed ({})", r.mod_id, r.from_version),
		_ => format!("`{}` {} → {}", r.mod_id, r.from_version, r.to_version),
	};
	let solo = r
		.solo_ratio
		.map(|s| format!(", {:.1}% across {} who changed only this", (1.0 - s) * 100.0, r.solo_pairs))
		.unwrap_or_default();
	format!(
		"{change} on {} {}: {} {:.1}% lower across {} players{solo}",
		r.minecraft_version,
		r.loader,
		r.metric,
		(1.0 - r.ratio) * 100.0,
		r.pairs,
	)
}

fn alert_message(fresh: &[perf_regression::Model]) -> String {
	let mut message = "**Performance regressions detected**".to_owned();
	let mut shown = 0;
	for line in fresh.iter().take(MAX_ALERT_LINES).map(describe) {
		if message.len() + 1 + line.len() > MAX_ALERT_LEN {
			break;
		}
		message.push('\n');
		message.push_str(&line);
		shown += 1;
	}
	if fresh.len() > shown {
		message.push_str(&format!("\n…and {} more in /v1/analytics/perf", fresh.len() - shown));
	}
	message
}

async fn notify(database: &DatabaseConnection, http: &reqwest::Client, webhook: &str) -> Result<(), DbErr> {
	let mut fresh = PerfRegression::update_many()
		.col_expr(perf_regression::Column::Notified, Expr::value(true))
		.filter(perf_regression::Column::Notified.eq(false))
		.exec_with_returning(database)
		.await?;
	if fresh.is_empty() {
		return Ok(());
	}
	fresh.sort_by(|a, b| b.solo_pairs.cmp(&a.solo_pairs).then(a.ratio.total_cmp(&b.ratio)));

	let message = alert_message(&fresh);
	warn!("{message}");

	let sent = http
		.post(webhook)
		.timeout(WEBHOOK_TIMEOUT)
		.json(&serde_json::json!({ "content": message }))
		.send()
		.await
		.and_then(reqwest::Response::error_for_status);
	if let Err(error) = sent {
		warn!("Unable to post performance regressions: {}", error.without_url());
		for r in &fresh {
			PerfRegression::update_many()
				.col_expr(perf_regression::Column::Notified, Expr::value(false))
				.filter(perf_regression::Column::MinecraftVersion.eq(&r.minecraft_version))
				.filter(perf_regression::Column::Loader.eq(&r.loader))
				.filter(perf_regression::Column::ModId.eq(&r.mod_id))
				.filter(perf_regression::Column::FromVersion.eq(&r.from_version))
				.filter(perf_regression::Column::ToVersion.eq(&r.to_version))
				.filter(perf_regression::Column::Metric.eq(&r.metric))
				.exec(database)
				.await?;
		}
		return Ok(());
	}
	info!("Reported {} performance regressions to the webhook", fresh.len());
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::collections::{BTreeMap, HashMap};

	use chrono::{TimeZone as _, Utc};

	use entities::perf_regression;

	use super::{
		Change, MAX_ALERT_LEN, PlayerPack, alert_message, attribute, consecutive, diff, median,
		transition,
	};

	fn mods(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
		entries.iter().map(|(id, v)| ((*id).to_owned(), (*v).to_owned())).collect()
	}

	fn row(player_id: i32, pack: &str, gpu: &str, day: u32, fps: f64) -> PlayerPack {
		PlayerPack {
			player_id,
			pack_hash: pack.to_owned(),
			gpu: gpu.to_owned(),
			cpu: "cpu".to_owned(),
			render_distance: 12,
			shaders: false,
			first_seen: Utc.with_ymd_and_hms(2026, 11, day, 0, 0, 0).single().expect("valid date").into(),
			median_fps: fps,
			low1_fps: fps / 2.0,
			low01_fps: fps / 3.0,
		}
	}

	fn change(id: &str, from: &str, to: &str) -> Change {
		Change { mod_id: id.to_owned(), from: from.to_owned(), to: to.to_owned() }
	}

	#[test]
	fn diff_covers_updates_additions_and_removals() {
		let before = mods(&[("sodium", "0.6"), ("old", "1"), ("same", "1")]);
		let after = mods(&[("sodium", "0.7"), ("new", "2"), ("same", "1")]);
		assert_eq!(diff(&before, &after), vec![
			change("new", "", "2"),
			change("old", "1", ""),
			change("sodium", "0.6", "0.7"),
		]);
	}

	#[test]
	fn pairs_packs_in_order_on_the_same_setup() {
		let rows = [
			row(1, "b", "gpu", 3, 90.0),
			row(1, "a", "gpu", 1, 100.0),
			row(1, "c", "other", 2, 500.0),
			row(2, "a", "gpu", 1, 100.0),
		];
		let pairs = consecutive(&rows);
		assert_eq!(pairs.len(), 1);
		assert_eq!((pairs[0].0.pack_hash.as_str(), pairs[0].1.pack_hash.as_str()), ("a", "b"));
	}

	#[test]
	fn blames_every_changed_mod_and_tracks_solo_changes() {
		let packs = HashMap::from([
			("a".to_owned(), mods(&[("sodium", "0.6"), ("polyplus", "2.4")])),
			("b".to_owned(), mods(&[("sodium", "0.7"), ("polyplus", "2.5")])),
			("c".to_owned(), mods(&[("sodium", "0.7"), ("polyplus", "2.4")])),
		]);
		let both = transition(&row(1, "a", "g", 1, 100.0), &row(1, "b", "g", 2, 80.0), &packs).expect("both packs differ");
		let solo = transition(&row(2, "a", "g", 1, 100.0), &row(2, "c", "g", 2, 90.0), &packs).expect("packs differ");
		let evidence = attribute(&[both, solo]);

		let sodium = &evidence[&change("sodium", "0.6", "0.7")];
		let mut all = sodium.all[0].clone();
		all.sort_by(f64::total_cmp);
		assert_eq!(all, vec![0.8, 0.9]);
		assert_eq!(sodium.solo[0], vec![0.9]);
		let polyplus = &evidence[&change("polyplus", "2.4", "2.5")];
		assert_eq!(polyplus.all[0], vec![0.8]);
		assert!(polyplus.solo[0].is_empty());
	}

	#[test]
	fn one_player_counts_once_per_change() {
		let packs = HashMap::from([
			("a".to_owned(), mods(&[("sodium", "0.6")])),
			("b".to_owned(), mods(&[("sodium", "0.7")])),
		]);
		let transitions = [("g1", 80.0), ("g2", 60.0), ("g3", 70.0)]
			.map(|(gpu, fps)| transition(&row(1, "a", gpu, 1, 100.0), &row(1, "b", gpu, 2, fps), &packs).expect("packs differ"));
		let evidence = attribute(&transitions);
		let sodium = &evidence[&change("sodium", "0.6", "0.7")];
		assert_eq!(sodium.all[0], vec![0.7]);
		assert_eq!(sodium.solo[0], vec![0.7]);
	}

	#[test]
	fn alert_fits_discord_limit() {
		let long = "x".repeat(128);
		let fresh: Vec<_> = (0..15)
			.map(|_| perf_regression::Model {
				minecraft_version: "1.21.11".to_owned(),
				loader: "fabric".to_owned(),
				mod_id: long.clone(),
				from_version: long.clone(),
				to_version: long.clone(),
				metric: "low01_fps".to_owned(),
				pairs: 20,
				ratio: 0.5,
				solo_pairs: 20,
				solo_ratio: Some(0.5),
				detected_at: Utc::now().into(),
				updated_at: Utc::now().into(),
				notified: false,
			})
			.collect();
		let message = alert_message(&fresh);
		assert!(message.chars().count() <= 2000);
		assert!(message.len() <= MAX_ALERT_LEN + 100);
		assert!(message.ends_with("more in /v1/analytics/perf"));
	}

	#[test]
	fn unknown_packs_are_skipped() {
		let packs = HashMap::new();
		assert!(transition(&row(1, "a", "g", 1, 100.0), &row(1, "b", "g", 2, 80.0), &packs).is_none());
	}

	#[test]
	fn median_of_even_and_odd() {
		assert_eq!(median(&[3.0, 1.0, 2.0]), Some((2.0, 3)));
		assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some((2.5, 4)));
		assert_eq!(median(&[f64::NAN]), None);
	}
}
