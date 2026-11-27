use std::{
	future::Future,
	io::{Cursor, Read},
};

use entities::{
	asset, asset_color_slot,
	prelude::{Asset, AssetColorSlot},
	sea_orm_active_enums::{AssetKind, CosmeticType},
};
use futures::{Stream, StreamExt};
use moka::future::Cache;
use reqwest::Url;
use s3::error::S3Error;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, Set};
use serde_json::Value;
use tokio::task::spawn_blocking;
use tracing::warn;
use zip::ZipArchive;

use super::limits::BundleLimits;
use crate::{api::ApiState, utils::zip::is_zip};

const MANIFEST: &str = "manifest.json";
const PALETTES: &str = "palettes.json";
const NAMEPLATE: &str = "nameplate";
const MAX_JSON_BYTES: u64 = 256 * 1024;
const SLOT_ID_MAX_LEN: usize = 64;

fn is_slot_id(id: &str) -> bool {
	(1..=SLOT_ID_MAX_LEN).contains(&id.len())
		&& id
			.bytes()
			.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn read_json(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<Value> {
	let entry = archive.by_name(name).ok()?;
	let mut text = Vec::new();
	entry.take(MAX_JSON_BYTES + 1).read_to_end(&mut text).ok()?;
	if text.len() as u64 > MAX_JSON_BYTES {
		return None;
	}
	let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&text);
	serde_json::from_slice(text).ok()
}

pub(crate) fn customizable_slots(bundle: &[u8]) -> Vec<String> {
	if !is_zip(bundle) {
		return Vec::new();
	}
	let Ok(mut archive) = ZipArchive::new(Cursor::new(bundle)) else {
		return Vec::new();
	};
	let Some(manifest) = read_json(&mut archive, MANIFEST) else {
		return Vec::new();
	};
	let nameplate = manifest.get("kind").and_then(Value::as_str) == Some(NAMEPLATE);
	if !nameplate && manifest.get("palettes") != Some(&Value::Bool(true)) {
		return Vec::new();
	}
	let Some(palettes) = read_json(&mut archive, PALETTES) else {
		return Vec::new();
	};
	let Some(slots) = palettes.get("slots").and_then(Value::as_array) else {
		return Vec::new();
	};
	let mut out: Vec<String> = Vec::new();
	for slot in slots {
		if slot.get("customizable") != Some(&Value::Bool(true)) {
			continue;
		}
		let Some(id) = slot.get("id").and_then(Value::as_str) else {
			continue;
		};
		if is_slot_id(id) && !out.iter().any(|known| known == id) {
			out.push(id.to_string());
		}
	}
	out
}

fn is_zip_path(path: &str) -> bool {
	path.split(['?', '#'])
		.next()
		.is_some_and(|path| path.ends_with(".zip"))
}

pub(crate) fn may_declare_slots(asset: &asset::Model) -> bool {
	asset.asset_kind == AssetKind::Bundle
		|| asset.storage_path.as_deref().is_some_and(is_zip_path)
		|| (asset.storage_path.is_none() && asset.url.as_deref().is_some_and(is_zip_path))
}

pub(crate) fn own_storage_url(url: &str, public_url: &str) -> Option<Url> {
	let url = Url::parse(url).ok()?;
	let public = Url::parse(public_url).ok()?;
	let same_origin = url.scheme() == "https"
		&& url.scheme() == public.scheme()
		&& url.host_str().is_some()
		&& url.host_str() == public.host_str()
		&& url.port_or_known_default() == public.port_or_known_default()
		&& url.username().is_empty()
		&& url.password().is_none();
	same_origin.then_some(url)
}

pub(crate) async fn store_slots(
	db: &impl ConnectionTrait,
	asset_id: i32,
	slots: Vec<String>,
) -> Result<(), DbErr> {
	if slots.is_empty() {
		return Ok(());
	}
	AssetColorSlot::insert_many(slots.into_iter().map(|slot| {
		asset_color_slot::ActiveModel {
			asset_id: Set(asset_id),
			slot: Set(slot),
		}
	}))
	.on_conflict_do_nothing()
	.exec(db)
	.await?;
	Ok(())
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum ScanError {
	#[error("Unable to query database: {0}")]
	Database(#[from] DbErr),
	#[error("{0}")]
	Unreadable(String),
}

pub(crate) async fn read_capped<S, B, E>(
	mut stream: S,
	cap: usize,
) -> Result<Option<Vec<u8>>, E>
where
	S: Stream<Item = Result<B, E>> + Unpin,
	B: AsRef<[u8]>,
{
	let mut data = Vec::new();
	while let Some(chunk) = stream.next().await {
		let chunk = chunk?;
		let chunk = chunk.as_ref();
		if data.len() + chunk.len() > cap {
			return Ok(None);
		}
		data.extend_from_slice(chunk);
	}
	Ok(Some(data))
}

async fn download_stored(
	state: &ApiState,
	path: &str,
	cap: usize,
) -> Result<Option<Vec<u8>>, ScanError> {
	let unreadable = |error: S3Error| ScanError::Unreadable(error.to_string());
	let mut response = state
		.s3_bucket
		.get_object_stream(path)
		.await
		.map_err(unreadable)?;
	if !(200..300).contains(&response.status_code) {
		return Err(ScanError::Unreadable(format!(
			"object storage answered {}",
			response.status_code
		)));
	}
	read_capped(response.bytes(), cap).await.map_err(unreadable)
}

async fn download_url(
	state: &ApiState,
	url: Url,
	cap: usize,
) -> Result<Option<Vec<u8>>, ScanError> {
	let unreadable = |error: reqwest::Error| ScanError::Unreadable(error.to_string());
	let response = state.asset_client.get(url).send().await.map_err(unreadable)?;
	if !response.status().is_success() {
		return Err(ScanError::Unreadable(format!(
			"the asset host answered {}",
			response.status()
		)));
	}
	if response.content_length().is_some_and(|length| length > cap as u64) {
		return Ok(None);
	}
	let chunks = futures::stream::unfold(Some(response), |response| async move {
		let mut response = response?;
		match response.chunk().await {
			Ok(Some(chunk)) => Some((Ok(chunk), Some(response))),
			Ok(None) => None,
			Err(error) => Some((Err(error), None)),
		}
	});
	read_capped(Box::pin(chunks), cap).await.map_err(unreadable)
}

async fn scan_asset(
	state: &ApiState,
	asset_id: i32,
	limits: BundleLimits,
) -> Result<Vec<String>, ScanError> {
	let Some(asset) = Asset::find_by_id(asset_id).one(&state.database).await? else {
		return Err(ScanError::Unreadable("the asset does not exist".to_owned()));
	};
	let data = match (&asset.storage_path, &asset.url) {
		_ if !may_declare_slots(&asset) => Some(Vec::new()),
		(Some(path), _) => download_stored(state, path, limits.download).await?,
		(None, Some(url)) => match own_storage_url(url, &state.s3_public_url) {
			Some(url) => download_url(state, url, limits.download).await?,
			None => {
				return Err(ScanError::Unreadable(format!(
					"its url {url} is not on our storage host"
				)));
			}
		},
		(None, None) => Some(Vec::new()),
	};
	let slots = match data {
		Some(data) if !data.is_empty() => {
			spawn_blocking(move || customizable_slots(&data))
				.await
				.unwrap_or_default()
		}
		Some(_) => Vec::new(),
		None => {
			warn!(
				"Asset {asset_id} is over {} bytes, so it has no colour slots",
				limits.download
			);
			Vec::new()
		}
	};
	store_slots(&state.database, asset_id, slots.clone()).await?;
	Ok(slots)
}

pub(crate) async fn coalesced_scan(
	scans: &Cache<i32, Vec<String>>,
	asset_id: i32,
	scan: impl Future<Output = Result<Vec<String>, ScanError>>,
) -> Result<Vec<String>, DbErr> {
	match scans.try_get_with(asset_id, scan).await {
		Ok(slots) => Ok(slots),
		Err(error) => match error.as_ref() {
			ScanError::Database(_) => Err(DbErr::Custom(error.to_string())),
			ScanError::Unreadable(reason) => {
				warn!("Unable to read asset {asset_id} for its colour slots: {reason}");
				Ok(Vec::new())
			}
		},
	}
}

pub(crate) async fn scan_stored_slots(
	state: &ApiState,
	asset_id: i32,
	kind: &CosmeticType,
) -> Result<Vec<String>, DbErr> {
	let limits = BundleLimits::of(kind);
	coalesced_scan(
		&state.color_slot_scans,
		asset_id,
		scan_asset(state, asset_id, limits),
	)
	.await
}

#[cfg(test)]
mod tests {
	use std::{
		convert::Infallible,
		sync::atomic::{AtomicUsize, Ordering},
	};

	use chrono::Utc;
	use entities::{asset, sea_orm_active_enums::AssetKind};
	use futures::stream;
	use moka::future::Cache;
	use sea_orm::DbErr;
	use serde_json::{Value, json};

	use super::{
		ScanError, coalesced_scan, customizable_slots, may_declare_slots, own_storage_url,
		read_capped,
	};
	use crate::api::v0::cosmetics::engine::tests::{SAMPLE, nested, zip};

	fn bundle(manifest: Value, palettes: &[u8]) -> Vec<u8> {
		let manifest = serde_json::to_vec(&manifest).unwrap();
		zip(&[("manifest.json", &manifest), ("palettes.json", palettes)])
	}

	fn item_with(slots: Value) -> Vec<u8> {
		let item = json!({ "format": 1, "kind": "item", "id": "wings", "palettes": true });
		bundle(item, &serde_json::to_vec(&json!({ "slots": slots })).unwrap())
	}

	fn slot(id: &str, customizable: Value) -> Value {
		json!({ "id": id, "ramp": ["#ff000000"], "customizable": customizable })
	}

	#[tokio::test]
	async fn scans_of_one_asset_run_once_and_failures_are_not_remembered() {
		let scans = Cache::new(16);
		let runs = AtomicUsize::new(0);
		let scan = || async {
			runs.fetch_add(1, Ordering::SeqCst);
			tokio::task::yield_now().await;
			Ok(vec!["fur".to_owned()])
		};
		let (first, second) = tokio::join!(coalesced_scan(&scans, 1, scan()), coalesced_scan(&scans, 1, scan()));
		assert_eq!((first.unwrap(), second.unwrap()), (vec!["fur".to_owned()], vec!["fur".to_owned()]));
		assert_eq!(coalesced_scan(&scans, 1, scan()).await.unwrap(), ["fur"]);
		assert_eq!(runs.load(Ordering::SeqCst), 1);

		let unreadable = coalesced_scan(&scans, 2, async { Err(ScanError::Unreadable("offline".to_owned())) });
		assert!(unreadable.await.unwrap().is_empty());
		let failed = coalesced_scan(&scans, 2, async { Err(ScanError::Database(DbErr::Custom("down".to_owned()))) });
		assert!(failed.await.is_err());
		assert_eq!(scans.get(&2).await, None);
	}

	#[tokio::test]
	async fn downloads_stop_past_the_cap() {
		let chunks = |sizes: &[usize]| {
			stream::iter(sizes.iter().map(|&size| Ok::<_, Infallible>(vec![0u8; size])).collect::<Vec<_>>())
		};
		assert_eq!(read_capped(chunks(&[4, 4]), 8).await.unwrap().map(|data| data.len()), Some(8));
		assert_eq!(read_capped(chunks(&[4, 4, 1]), 8).await.unwrap(), None);
		assert_eq!(read_capped(chunks(&[9]), 8).await.unwrap(), None);
		assert_eq!(read_capped(chunks(&[]), 8).await.unwrap(), Some(Vec::new()));

		let polled = AtomicUsize::new(0);
		let endless = stream::repeat_with(|| {
			polled.fetch_add(1, Ordering::SeqCst);
			Ok::<_, Infallible>(vec![0u8; 4])
		});
		assert_eq!(read_capped(endless, 10).await.unwrap(), None);
		assert_eq!(polled.load(Ordering::SeqCst), 3);
	}

	#[test]
	fn only_bundles_on_our_storage_declare_slots() {
		let asset = |kind, path: Option<&str>, url: Option<&str>| {
			let now = Utc::now().fixed_offset();
			let (storage_path, url) = (path.map(str::to_owned), url.map(str::to_owned));
			let (id, content_type, hash) = (1, None, None);
			asset::Model { id, storage_path, url, asset_kind: kind, content_type, hash, created_at: now, updated_at: now }
		};
		for (kind, path, url, declares) in [
			(AssetKind::Bundle, Some("nameplates/a.zip"), None, true),
			(AssetKind::Image, Some("wings/a.zip"), None, true),
			(AssetKind::Image, Some("capes/a.png"), None, false),
			(AssetKind::Image, None, None, false),
			(AssetKind::Image, None, Some("https://cdn.example.com/wings/a.zip?v=2"), true),
			(AssetKind::Bundle, None, Some("https://cdn.example.com/wings/a"), true),
			(AssetKind::Image, None, Some("https://cdn.example.com/capes/a.png"), false),
			(AssetKind::Image, Some("capes/a.png"), Some("https://cdn.example.com/wings/a.zip"), false),
		] {
			assert_eq!(may_declare_slots(&asset(kind, path, url)), declares, "{path:?} {url:?}");
		}

		let public = "https://cdn.example.com";
		for url in ["https://cdn.example.com/wings/a.zip", "https://CDN.example.com:443/a.zip"] {
			assert!(own_storage_url(url, public).is_some(), "{url}");
		}
		assert!(own_storage_url("https://cdn.example.com/a.zip", "https://cdn.example.com/assets").is_some());
		for url in [
			"http://cdn.example.com/a.zip",
			"https://cdn.example.com:8443/a.zip",
			"https://evil.example.com/a.zip",
			"https://cdn.example.com.evil.net/a.zip",
			"https://user@cdn.example.com/a.zip",
			"https://169.254.169.254/latest",
			"not a url",
		] {
			assert!(own_storage_url(url, public).is_none(), "{url}");
		}
		assert!(own_storage_url("http://localhost/a.zip", "http://localhost").is_none());
	}

	#[test]
	fn customizable_slots_are_read_from_the_palettes_in_order() {
		assert_eq!(customizable_slots(&zip(SAMPLE)), ["feather"]);
		let listed = item_with(json!([
			slot("trim", json!(true)),
			slot("eyes", json!(false)),
			{ "id": "body", "ramp": ["#ff000000"] },
			slot("glow", json!(true)),
			slot("trim", json!(true))
		]));
		assert_eq!(customizable_slots(&listed), ["trim", "glow"]);
		let odd = item_with(json!([
			slot("trim", json!("true")),
			slot("Trim", json!(true)),
			slot("", json!(true)),
			{ "customizable": true },
			slot("ok_1", json!(true))
		]));
		assert_eq!(customizable_slots(&odd), ["ok_1"]);

		let palettes = serde_json::to_vec(&json!({ "slots": [slot("ink", json!(true))] })).unwrap();
		let plate = json!({ "format": 1, "kind": "nameplate", "id": "plate" });
		assert_eq!(customizable_slots(&bundle(plate, &palettes)), ["ink"]);
		let with_bom = [b"\xEF\xBB\xBF".as_slice(), &palettes].concat();
		let item = json!({ "format": 1, "kind": "item", "id": "wings", "palettes": true });
		assert_eq!(customizable_slots(&bundle(item, &with_bom)), ["ink"]);
		for flag in [json!(false), Value::Null] {
			let mut manifest = json!({ "format": 1, "kind": "item", "id": "wings" });
			if !flag.is_null() {
				manifest["palettes"] = flag;
			}
			assert!(customizable_slots(&bundle(manifest, &palettes)).is_empty());
		}

		let item = json!({ "format": 1, "kind": "item", "id": "wings", "palettes": true });
		for unreadable in [
			b"\x89PNG\r\n\x1a\n".to_vec(),
			b"PK\x03\x04broken".to_vec(),
			zip(&[("palettes.json", b"{}")]),
			nested("wings/", SAMPLE),
			bundle(item, b"{"),
			item_with(json!({ "trim": {} })),
		] {
			assert!(customizable_slots(&unreadable).is_empty());
		}
	}
}
