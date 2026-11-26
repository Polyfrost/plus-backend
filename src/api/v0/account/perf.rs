use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, NoContent},
};
use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use entities::{perf_pack, perf_report, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, EntityTrait, PaginatorTrait as _, QueryFilter as _, Set,
	sea_query::OnConflict,
};
use serde::Deserialize;

use crate::api::{
	ApiState,
	state::ALL_VERSIONS,
	v0::account::{AuthenticatedPlayer, ClientKind, mods::valid_version},
};
use crate::utils::hash::sha256_hex;

const MAX_TEXT_LEN: usize = 128;
const MAX_MODS: usize = 1000;
const MAX_MOD_FIELD_LEN: usize = 128;
const MAX_FPS: f64 = 10_000.0;
const MIN_SECONDS: f64 = 30.0;
const MAX_SECONDS: f64 = 3600.0;
const MIN_REPORT_INTERVAL: Duration = Duration::minutes(4);

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum PerfError {
	#[error("Unable to record performance report: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Invalid performance report: {0}")]
	Invalid(&'static str),
}

impl IntoResponse for PerfError {
	fn into_response(self) -> axum::response::Response {
		let status = match self {
			Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			Self::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
		};
		crate::api::error_response(status, self)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RequestBody {
	client_version: String,
	minecraft_version: String,
	loader: String,
	os: String,
	os_version: Option<String>,
	java_version: Option<String>,
	cpu: Option<String>,
	cpu_cores: i32,
	ram_mb: Option<i64>,
	heap_mb: i64,
	gpu: Option<String>,
	render_distance: Option<i32>,
	max_fps: Option<i32>,
	vsync: Option<bool>,
	shaders: Option<bool>,
	mod_count: i32,
	pack_hash: String,
	mods: Option<BTreeMap<String, String>>,
	seconds: f64,
	frames: i32,
	avg_fps: f64,
	median_fps: f64,
	low1_fps: f64,
	low01_fps: f64,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("postPlayerPerf")
		.summary("Report in-game performance")
		.description(
			"Records a summary of the authorized player's recent frame times along with \
			 their hardware. Reports arriving within four minutes of the last one are \
			 dropped.",
		)
		.tag("account")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/perf", post_with(self::endpoint, self::endpoint_doc))
}

fn text(value: String) -> Option<String> {
	let value = value.trim();
	(!value.is_empty() && value.len() <= MAX_TEXT_LEN && !value.chars().any(char::is_control))
		.then(|| value.to_owned())
}

fn pack_hash(mods: &BTreeMap<String, String>) -> String {
	let canonical = mods
		.iter()
		.map(|(id, version)| format!("{id}@{version}"))
		.collect::<Vec<_>>()
		.join("\n");
	sha256_hex(canonical.as_bytes())
}

fn valid_mods(mods: &BTreeMap<String, String>) -> bool {
	let field = |value: &str| {
		!value.is_empty()
			&& value.len() <= MAX_MOD_FIELD_LEN
			&& !value.chars().any(char::is_control)
	};
	mods.len() <= MAX_MODS && mods.iter().all(|(id, version)| field(id) && field(version))
}

fn fps(value: f64) -> bool {
	value.is_finite() && value > 0.0 && value <= MAX_FPS
}

fn validate(
	mut body: RequestBody,
	player_id: i32,
) -> Result<(perf_report::ActiveModel, Option<perf_pack::ActiveModel>), PerfError> {
	if body.pack_hash.len() != 64 || !body.pack_hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
		return Err(PerfError::Invalid("pack_hash"));
	}
	let pack = match body.mods.take() {
		Some(mods) if !valid_mods(&mods) => return Err(PerfError::Invalid("mods")),
		Some(mods) if pack_hash(&mods) != body.pack_hash => {
			return Err(PerfError::Invalid("pack_hash does not match mods"));
		}
		Some(mods) => Some(perf_pack::ActiveModel {
			pack_hash: Set(body.pack_hash.clone()),
			mods: Set(serde_json::to_value(mods).unwrap_or_default()),
			..Default::default()
		}),
		None => None,
	};
	if !valid_version(&body.client_version)
		|| !valid_version(&body.minecraft_version)
		|| body.minecraft_version == ALL_VERSIONS
	{
		return Err(PerfError::Invalid("version"));
	}
	if !(body.seconds.is_finite() && (MIN_SECONDS..=MAX_SECONDS).contains(&body.seconds)) {
		return Err(PerfError::Invalid("seconds"));
	}
	if body.frames <= 0 || f64::from(body.frames) > body.seconds * MAX_FPS {
		return Err(PerfError::Invalid("frames"));
	}
	if ![body.avg_fps, body.median_fps, body.low1_fps, body.low01_fps]
		.into_iter()
		.all(fps)
	{
		return Err(PerfError::Invalid("fps"));
	}
	if !(1..=1024).contains(&body.cpu_cores) || body.heap_mb <= 0 || body.mod_count < 0 {
		return Err(PerfError::Invalid("hardware"));
	}

	Ok((perf_report::ActiveModel {
		player_id: Set(player_id),
		client_version: Set(body.client_version),
		minecraft_version: Set(body.minecraft_version),
		loader: Set(text(body.loader).ok_or(PerfError::Invalid("loader"))?),
		os: Set(text(body.os).ok_or(PerfError::Invalid("os"))?),
		os_version: Set(body.os_version.and_then(text)),
		java_version: Set(body.java_version.and_then(text)),
		cpu: Set(body.cpu.and_then(text)),
		cpu_cores: Set(body.cpu_cores),
		ram_mb: Set(body.ram_mb.filter(|mb| *mb > 0)),
		heap_mb: Set(body.heap_mb),
		gpu: Set(body.gpu.and_then(text)),
		render_distance: Set(body.render_distance.filter(|d| (0..=256).contains(d))),
		max_fps: Set(body.max_fps.filter(|f| *f > 0)),
		vsync: Set(body.vsync),
		shaders: Set(body.shaders),
		mod_count: Set(body.mod_count),
		pack_hash: Set(body.pack_hash),
		seconds: Set(body.seconds),
		frames: Set(body.frames),
		avg_fps: Set(body.avg_fps),
		median_fps: Set(body.median_fps),
		low1_fps: Set(body.low1_fps),
		low01_fps: Set(body.low01_fps),
		..Default::default()
	}, pack))
}

#[tracing::instrument(level = "debug", skip(state, body))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	client: ClientKind,
	Json(body): Json<RequestBody>,
) -> Result<NoContent, PerfError> {
	if client != ClientKind::Game {
		return Ok(NoContent);
	}
	let (report, pack) = validate(body, player.id)?;
	if let Some(pack) = pack {
		PerfPack::insert(pack)
			.on_conflict(OnConflict::column(perf_pack::Column::PackHash).do_nothing().to_owned())
			.do_nothing()
			.exec_without_returning(&state.database)
			.await?;
	}

	let recent = PerfReport::find()
		.filter(perf_report::Column::PlayerId.eq(player.id))
		.filter(perf_report::Column::ReportedAt.gt(Utc::now() - MIN_REPORT_INTERVAL))
		.count(&state.database)
		.await?;
	if recent > 0 {
		return Ok(NoContent);
	}

	PerfReport::insert(report).exec_without_returning(&state.database).await?;

	Ok(NoContent)
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::pack_hash;

	#[test]
	fn pack_hash_matches_client_format() {
		let mods = BTreeMap::from([
			("a-b".to_owned(), "1".to_owned()),
			("a".to_owned(), "2.0+mc1.21".to_owned()),
		]);
		assert_eq!(
			pack_hash(&mods),
			"fcf4abafd64ad5196c140f3f56261f73d1c4f3eea0e259c46ab5841f42d0374d"
		);
	}
}
