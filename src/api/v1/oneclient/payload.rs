use std::{
	collections::{BTreeSet, HashSet},
	fs::File,
	io::{self, BufReader, BufWriter, Read, Write},
	path::Path,
	sync::LazyLock,
};

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha512};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

use super::MAX_PAYLOAD_BYTES;

const INDEX_ENTRY: &str = "modrinth.index.json";
const SHARE_ENTRY: &str = "oneclient.cluster.json";
const OVERRIDES: &str = "overrides/";
const ICON_STEM: &str = "icon.";
const ICON_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

const MAX_ENTRIES: usize = 20_000;
const MAX_ENTRY_NAME: usize = 512;
const MAX_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SHARE_BYTES: u64 = 1024 * 1024;
const MAX_ICON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SETTING_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SETTINGS_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SETTING_FILES: usize = 4000;
const MAX_REPORTED: usize = 50;

const CONTENT_DIRS: &[&str] = &[
	"mods",
	"mod",
	"resourcepacks",
	"resourcepack",
	"shaderpacks",
	"shaders",
	"shader",
];
const SETTING_DIRS: &[&str] = &["config", "oneconfig"];
const SETTING_FILES: &[&str] = &["options.txt", "optionsof.txt", "optionsshaders.txt"];
const SKIPPED_DIRS: &[&str] = &[
	"oneconfig/backup",
	"oneconfig/translations",
	"oneconfig/sounds",
];
const SKIPPED_EXTENSIONS: &[&str] = &[
	"jar", "zip", "gz", "7z", "rar", "log", "tmp", "lock", "bak", "old", "dll", "so",
	"dylib", "exe", "db", "sqlite", "mca", "dat_old",
];
const PRIVATE_MARKERS: &[&str] = &[
	"token",
	"secret",
	"password",
	"passwd",
	"credential",
	"session",
	"auth",
	"apikey",
	"api_key",
	"api-key",
	"account",
	"cookie",
	"private",
];
const INDEX_HOSTS: &[&str] = &[
	"cdn.modrinth.com",
	"github.com",
	"raw.githubusercontent.com",
	"gitlab.com",
	"edge.forgecdn.net",
	"mediafilez.forgecdn.net",
];
const LOADERS: &[&str] = &[
	"minecraft",
	"Forge",
	"NeoForge",
	"Quilt",
	"Fabric",
	"Ornithe",
];
const KINDS: &[&str] = &["one_client", "vanilla", "modded"];

static KNOWN_SECRETS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
	[
		("private key", r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----"),
		(
			"JSON web token",
			r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
		),
		(
			"GitHub token",
			r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,})",
		),
		("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}"),
		("AWS key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
		("Google API key", r"\bAIza[0-9A-Za-z_-]{35}"),
		("Slack token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}"),
		("Stripe key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{16,}"),
		("API key", r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{32,}"),
		(
			"Discord token",
			r"\b[MNO][A-Za-z0-9_-]{23,27}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,40}\b",
		),
		(
			"Discord webhook",
			r"(?i)discord(?:app)?\.com/api/webhooks/\d+/[A-Za-z0-9_-]{20,}",
		),
		("Microsoft token", r"\bM\.[A-Z]\d{3}_[A-Za-z0-9_.*!$-]{20,}"),
		("Xbox token", r"XBL3\.0 x=\d+;"),
		("bearer token", r"(?i)\bbearer\s+[A-Za-z0-9_\-.=+/]{20,}"),
		(
			"credentials in a URL",
			r"(?i)\b[a-z][a-z0-9+.-]*://[^/\s:@\x22']+:[^/\s@\x22']+@",
		),
	]
	.into_iter()
	.map(|(label, pattern)| (label, Regex::new(pattern).expect("valid secret pattern")))
	.collect()
});

static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(
		r#"(?i)(?P<key>[a-z0-9_.\-]*(?:pass(?:word|wd|phrase)|pwd|secret|credential|token|api[_.\-]?key|access[_.\-]?key|private[_.\-]?key|auth[_.\-]?(?:key|code)|session[_.\-]?(?:id|key)|cookie)[a-z0-9_.\-]*)["']?\s*[:=]\s*(?:"(?P<dq>[^"\r\n]*)"|'(?P<sq>[^'\r\n]*)'|(?P<bare>[^\s,;}\]\[#"'<>]+))"#,
	)
	.expect("valid assignment pattern")
});

const STRONG_KEY_SUFFIXES: &[&str] = &[
	"password",
	"passwd",
	"passphrase",
	"pwd",
	"secret",
	"secretkey",
	"secret_key",
	"secret-key",
	"credential",
	"credentials",
];

#[derive(Debug, Clone)]
pub(super) struct Declared {
	pub(super) format: i32,
	pub(super) name: String,
	pub(super) mc_version: String,
	pub(super) mc_loader: String,
	pub(super) kind: String,
	pub(super) size: i64,
	pub(super) sha512: String,
}

#[derive(Debug)]
pub(super) struct Sanitized {
	pub(super) size: i64,
	pub(super) sha512: String,
	pub(super) removed: Vec<String>,
	pub(super) rewritten: bool,
}

#[derive(thiserror::Error, Debug)]
pub(super) enum PayloadError {
	#[error("{0}")]
	Invalid(String),
	#[error("The instance contains something that looks like a secret ({reason})")]
	Secret { reason: &'static str },
	#[error("Unable to read the payload: {0}")]
	Io(#[from] io::Error),
}

impl From<zip::result::ZipError> for PayloadError {
	fn from(error: zip::result::ZipError) -> Self {
		match error {
			zip::result::ZipError::Io(error) => Self::Io(error),
			other => {
				Self::Invalid(format!("The payload is not a valid zip file: {other}"))
			}
		}
	}
}

fn invalid(message: impl Into<String>) -> PayloadError {
	PayloadError::Invalid(message.into())
}

pub(super) fn is_known_loader(loader: &str) -> bool {
	LOADERS.contains(&loader)
}

pub(super) fn is_known_kind(kind: &str) -> bool {
	KINDS.contains(&kind)
}

pub(super) fn sha512_file(path: &Path) -> io::Result<(u64, String)> {
	let mut reader = BufReader::new(File::open(path)?);
	let mut hasher = Sha512::new();
	let mut buffer = vec![0; 256 * 1024];
	let mut size = 0;
	loop {
		let read = reader.read(&mut buffer)?;
		if read == 0 {
			break;
		}
		size += read as u64;
		hasher.update(&buffer[..read]);
	}
	Ok((size, crate::utils::hash::hex(hasher.finalize())))
}

enum Entry {
	Keep,
	Skip,
	Index,
	Share,
}

pub(super) fn sanitize(
	input: &Path,
	output: &Path,
	declared: &Declared,
) -> Result<Sanitized, PayloadError> {
	let (size, sha512) = sha512_file(input)?;
	if size > MAX_PAYLOAD_BYTES {
		return Err(invalid("The payload is over the size limit"));
	}
	if size as i64 != declared.size || sha512 != declared.sha512 {
		return Err(invalid(
			"The uploaded payload does not match the declared size and sha512",
		));
	}

	let mut archive = ZipArchive::new(BufReader::new(File::open(input)?))?;
	if archive.len() > MAX_ENTRIES {
		return Err(invalid("The payload has too many entries"));
	}

	let mut plan = Vec::with_capacity(archive.len());
	let mut names = HashSet::with_capacity(archive.len());
	let mut removed = BTreeSet::new();
	let mut uncompressed = 0u64;
	let mut setting_files = 0usize;
	let mut setting_bytes = 0u64;
	let mut index = None;
	let mut share = None;

	for position in 0..archive.len() {
		let entry = archive.by_index_raw(position)?;
		let name = entry.name().to_string();
		if !is_safe_entry_name(&name) || entry.enclosed_name().is_none() {
			return Err(invalid("The payload contains an unsafe path"));
		}
		if !names.insert(name.clone()) {
			return Err(invalid("The payload contains a path twice"));
		}
		if entry.encrypted() || entry.is_symlink() {
			return Err(invalid("The payload contains an encrypted file or a link"));
		}
		uncompressed = uncompressed.saturating_add(entry.size());
		if uncompressed > MAX_UNCOMPRESSED_BYTES {
			return Err(invalid("The payload expands to more than the size limit"));
		}
		let entry_size = entry.size();
		let is_dir = entry.is_dir();
		drop(entry);

		if is_dir {
			plan.push(Entry::Skip);
			continue;
		}

		match name.as_str() {
			INDEX_ENTRY => {
				let bytes = read_entry(&mut archive, position, MAX_INDEX_BYTES)?;
				index = Some(
					serde_json::from_slice::<Value>(&bytes)
						.map_err(|_| invalid("The modpack index is not valid JSON"))?,
				);
				plan.push(Entry::Index);
				continue;
			}
			SHARE_ENTRY => {
				let bytes = read_entry(&mut archive, position, MAX_SHARE_BYTES)?;
				share = Some(serde_json::from_slice::<Value>(&bytes).map_err(|_| {
					invalid("The instance description is not valid JSON")
				})?);
				plan.push(Entry::Share);
				continue;
			}
			_ => {}
		}

		if let Some(extension) = name.strip_prefix(ICON_STEM) {
			if !ICON_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
				return Err(invalid("The payload contains an unexpected file"));
			}
			if entry_size > MAX_ICON_BYTES {
				return Err(invalid("The instance icon is too large"));
			}
			plan.push(Entry::Keep);
			continue;
		}

		let Some(relative) = name.strip_prefix(OVERRIDES) else {
			return Err(invalid("The payload contains an unexpected file"));
		};

		if is_content_path(relative) {
			plan.push(Entry::Keep);
			continue;
		}

		if !is_setting_path(relative)
			|| entry_size > MAX_SETTING_FILE_BYTES
			|| setting_files >= MAX_SETTING_FILES
			|| setting_bytes + entry_size > MAX_SETTINGS_BYTES
		{
			removed.insert(relative.to_string());
			plan.push(Entry::Skip);
			continue;
		}

		let bytes = read_entry(&mut archive, position, MAX_SETTING_FILE_BYTES)?;
		if find_secret(&String::from_utf8_lossy(&bytes)).is_some() {
			removed.insert(relative.to_string());
			plan.push(Entry::Skip);
			continue;
		}
		setting_files += 1;
		setting_bytes += bytes.len() as u64;
		plan.push(Entry::Keep);
	}

	let mut index = index.ok_or_else(|| invalid("The payload has no modpack index"))?;
	let mut share =
		share.ok_or_else(|| invalid("The payload is not a OneClient instance"))?;

	check_share_matches(&share, declared)?;
	let index_changed = sanitize_index(&mut index, &mut removed)?;
	let share_changed = sanitize_share(&mut share, &removed)?;
	reject_secrets_in(&index)?;
	reject_secrets_in(&share)?;

	let removed: Vec<String> = removed.into_iter().collect();
	let skipped_files = plan.iter().any(|entry| matches!(entry, Entry::Skip));
	if removed.is_empty() && !index_changed && !share_changed && !skipped_files {
		return Ok(Sanitized {
			size: size as i64,
			sha512,
			removed,
			rewritten: false,
		});
	}

	write_archive(&mut archive, &plan, &index, &share, output)?;
	let (size, sha512) = sha512_file(output)?;
	if size > MAX_PAYLOAD_BYTES {
		return Err(invalid("The payload is over the size limit"));
	}

	Ok(Sanitized {
		size: size as i64,
		sha512,
		removed: removed.into_iter().take(MAX_REPORTED).collect(),
		rewritten: true,
	})
}

fn read_entry<R: io::Read + io::Seek>(
	archive: &mut ZipArchive<R>,
	position: usize,
	limit: u64,
) -> Result<Vec<u8>, PayloadError> {
	let entry = archive.by_index(position)?;
	if entry.size() > limit {
		return Err(invalid(format!("{} is too large", entry.name())));
	}
	let mut bytes = Vec::with_capacity(entry.size() as usize);
	entry.take(limit + 1).read_to_end(&mut bytes)?;
	if bytes.len() as u64 > limit {
		return Err(invalid("A file in the payload is larger than it claims"));
	}
	Ok(bytes)
}

fn write_archive<R: io::Read + io::Seek>(
	archive: &mut ZipArchive<R>,
	plan: &[Entry],
	index: &Value,
	share: &Value,
	output: &Path,
) -> Result<(), PayloadError> {
	let mut writer = ZipWriter::new(BufWriter::new(File::create(output)?));
	let options =
		SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

	for (position, action) in plan.iter().enumerate() {
		match action {
			Entry::Skip => {}
			Entry::Keep => writer.raw_copy_file(archive.by_index_raw(position)?)?,
			Entry::Index => {
				writer.start_file(INDEX_ENTRY, options)?;
				writer
					.write_all(&serde_json::to_vec(index).map_err(io::Error::other)?)?;
			}
			Entry::Share => {
				writer.start_file(SHARE_ENTRY, options)?;
				writer
					.write_all(&serde_json::to_vec(share).map_err(io::Error::other)?)?;
			}
		}
	}

	writer.finish()?.flush()?;
	Ok(())
}

fn check_share_matches(share: &Value, declared: &Declared) -> Result<(), PayloadError> {
	let text = |key: &str| share.get(key).and_then(Value::as_str);
	let name: String = text("name")
		.unwrap_or_default()
		.chars()
		.take(super::MAX_NAME_CHARS)
		.collect();
	let format = share.get("format").and_then(Value::as_i64);

	if format != Some(i64::from(declared.format))
		|| name != declared.name
		|| text("mc_version") != Some(declared.mc_version.as_str())
		|| text("mc_loader") != Some(declared.mc_loader.as_str())
		|| text("kind") != Some(declared.kind.as_str())
	{
		return Err(invalid(
			"The instance description does not match the declared name, version, \
			 loader or kind",
		));
	}
	Ok(())
}

fn sanitize_index(
	index: &mut Value,
	removed: &mut BTreeSet<String>,
) -> Result<bool, PayloadError> {
	let Some(files) = index.get_mut("files") else {
		return Ok(false);
	};
	let files = files
		.as_array_mut()
		.ok_or_else(|| invalid("The modpack index has no file list"))?;

	let before = files.len();
	let mut problem = None;
	files.retain(|file| {
		let path = file.get("path").and_then(Value::as_str).unwrap_or_default();
		if !is_safe_entry_name(path) {
			problem = Some("The modpack index lists an unsafe path");
			return false;
		}
		let urls_ok =
			file.get("downloads")
				.and_then(Value::as_array)
				.is_some_and(|urls| {
					!urls.is_empty()
						&& urls.iter().all(|url| {
							url.as_str().is_some_and(|url| is_safe_url(url, true))
						})
				});
		let keep = urls_ok && (is_content_path(path) || is_setting_path(path));
		if !keep {
			removed.insert(path.to_string());
		}
		keep
	});

	if let Some(problem) = problem {
		return Err(invalid(problem));
	}
	Ok(files.len() != before)
}

fn sanitize_share(
	share: &mut Value,
	removed: &BTreeSet<String>,
) -> Result<bool, PayloadError> {
	let mut changed = false;

	if let Some(settings) = share.get_mut("settings").and_then(Value::as_array_mut) {
		let before = settings.len();
		settings.retain(|path| path.as_str().is_some_and(|path| !removed.contains(path)));
		changed |= settings.len() != before;
	}

	if let Some(downloads) = share.get_mut("downloads").and_then(Value::as_array_mut) {
		let before = downloads.len();
		downloads.retain(|download| {
			let path = download
				.get("path")
				.and_then(Value::as_str)
				.unwrap_or_default();
			let url = download
				.get("url")
				.and_then(Value::as_str)
				.unwrap_or_default();
			is_safe_entry_name(path) && is_content_path(path) && is_safe_url(url, false)
		});
		changed |= downloads.len() != before;
	}

	Ok(changed)
}

fn reject_secrets_in(value: &Value) -> Result<(), PayloadError> {
	let text = serde_json::to_string(value).map_err(io::Error::other)?;
	match find_secret(&text) {
		Some(reason) => Err(PayloadError::Secret { reason }),
		None => Ok(()),
	}
}

fn is_safe_entry_name(name: &str) -> bool {
	!name.is_empty()
		&& name.len() <= MAX_ENTRY_NAME
		&& !name.starts_with('/')
		&& !name.contains(['\\', ':', '\0'])
		&& !name.chars().any(char::is_control)
		&& name.trim_end_matches('/').split('/').all(|segment| {
			!segment.is_empty()
				&& segment != "."
				&& segment != ".."
				&& !segment.ends_with(['.', ' '])
		})
}

fn is_content_path(path: &str) -> bool {
	let Some((folder, file)) = path.split_once('/') else {
		return false;
	};
	!file.is_empty()
		&& !file.contains('/')
		&& CONTENT_DIRS.contains(&folder.to_ascii_lowercase().as_str())
}

pub(super) fn is_setting_path(path: &str) -> bool {
	if !is_safe_entry_name(path) || is_content_path(path) {
		return false;
	}
	let lower = path.to_ascii_lowercase();
	if SETTING_FILES.contains(&lower.as_str()) {
		return true;
	}
	let Some((root, _)) = lower.split_once('/') else {
		return false;
	};
	if !SETTING_DIRS.contains(&root) {
		return false;
	}
	if SKIPPED_DIRS
		.iter()
		.any(|dir| lower == *dir || lower.starts_with(&format!("{dir}/")))
	{
		return false;
	}
	if lower.split('/').any(|segment| segment.starts_with('.')) {
		return false;
	}
	let name = lower.rsplit('/').next().unwrap_or_default();
	if PRIVATE_MARKERS.iter().any(|marker| name.contains(marker)) {
		return false;
	}
	!name
		.rsplit_once('.')
		.is_some_and(|(_, extension)| SKIPPED_EXTENSIONS.contains(&extension))
}

fn is_safe_url(url: &str, index: bool) -> bool {
	let Ok(parsed) = reqwest::Url::parse(url) else {
		return false;
	};
	let scheme_ok = match parsed.scheme() {
		"https" => true,
		"http" => !index,
		_ => false,
	};
	let host_ok = parsed.host_str().is_some_and(|host| {
		!index || INDEX_HOSTS.contains(&host.to_ascii_lowercase().as_str())
	});
	let query_ok = parsed.query_pairs().all(|(key, _)| {
		let key = key.to_ascii_lowercase();
		!PRIVATE_MARKERS.iter().any(|marker| key.contains(marker))
			&& !["sig", "signature", "key", "x-amz-credential"].contains(&key.as_str())
	});
	scheme_ok
		&& host_ok
		&& parsed.username().is_empty()
		&& parsed.password().is_none()
		&& query_ok
		&& find_secret(url).is_none()
}

pub(super) fn find_secret(text: &str) -> Option<&'static str> {
	for (label, pattern) in KNOWN_SECRETS.iter() {
		if pattern.is_match(text) {
			return Some(label);
		}
	}
	for captures in ASSIGNMENT.captures_iter(text) {
		let key = captures.name("key").map_or("", |key| key.as_str());
		let value = captures
			.name("dq")
			.or_else(|| captures.name("sq"))
			.or_else(|| captures.name("bare"))
			.map_or("", |value| value.as_str());
		if looks_secret(key, value) {
			return Some("a password, token or key in the settings");
		}
	}
	None
}

fn looks_secret(key: &str, value: &str) -> bool {
	let value = value.trim();
	if value.chars().count() < 4 {
		return false;
	}
	let lower = value.to_ascii_lowercase();
	if matches!(
		lower.as_str(),
		"true" | "false" | "null" | "none" | "nil" | "undefined" | "default" | "empty"
	) || value.parse::<f64>().is_ok()
		|| is_color(value)
		|| is_placeholder(value)
	{
		return false;
	}

	let key = key.to_ascii_lowercase();
	if STRONG_KEY_SUFFIXES
		.iter()
		.any(|suffix| key.ends_with(suffix))
	{
		return true;
	}

	value.len() >= 16
		&& value.chars().any(|c| c.is_ascii_digit())
		&& value.chars().any(|c| c.is_ascii_alphabetic())
		&& value
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || "-_.+/=:".contains(c))
}

fn is_color(value: &str) -> bool {
	let hex = value
		.strip_prefix('#')
		.or_else(|| value.strip_prefix("0x"))
		.or_else(|| value.strip_prefix("0X"));
	hex.is_some_and(|hex| {
		matches!(hex.len(), 3 | 4 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
	})
}

fn is_placeholder(value: &str) -> bool {
	let mut chars = value.chars();
	let first = chars.next().unwrap_or_default();
	(value.starts_with('<') && value.ends_with('>'))
		|| (value.starts_with("${") && value.ends_with('}'))
		|| chars.all(|c| c == first)
}

#[cfg(test)]
mod tests {
	use std::io::Write;

	use super::*;

	fn declared(path: &Path) -> Declared {
		let (size, sha512) = sha512_file(path).expect("test setup");
		Declared {
			format: 1,
			name: "Skyblock".into(),
			mc_version: "1.21.1".into(),
			mc_loader: "Fabric".into(),
			kind: "modded".into(),
			size: size as i64,
			sha512,
		}
	}

	fn share_json() -> String {
		serde_json::json!({
			"format": 1,
			"name": "Skyblock",
			"kind": "modded",
			"mc_version": "1.21.1",
			"mc_loader": "Fabric",
			"settings": ["config/hytils.toml", "options.txt"],
		})
		.to_string()
	}

	fn index_json() -> String {
		serde_json::json!({
			"formatVersion": 1,
			"game": "minecraft",
			"files": [{
				"path": "mods/sodium.jar",
				"hashes": {"sha1": "0", "sha512": "0"},
				"downloads": ["https://cdn.modrinth.com/data/AANobbMI/sodium.jar"],
				"fileSize": 1,
			}],
		})
		.to_string()
	}

	fn write_zip(path: &Path, entries: &[(&str, &str)]) {
		let mut writer = ZipWriter::new(File::create(path).expect("test setup"));
		for (name, content) in entries {
			writer
				.start_file(*name, SimpleFileOptions::default())
				.expect("test setup");
			writer.write_all(content.as_bytes()).expect("test setup");
		}
		writer.finish().expect("test setup");
	}

	fn scratch(name: &str) -> std::path::PathBuf {
		std::env::temp_dir().join(format!(
			"oneclient-sync-test-{}-{name}",
			uuid::Uuid::now_v7().simple()
		))
	}

	#[test]
	fn recognizes_common_secret_formats() {
		assert!(find_secret("-----BEGIN RSA PRIVATE KEY-----").is_some());
		assert!(
			find_secret(
				"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N"
			)
			.is_some()
		);
		assert!(find_secret("ghp_abcdefghijklmnopqrstuvwxyz0123456789").is_some());
		assert!(find_secret("AKIAIOSFODNN7EXAMPLE").is_some());
		assert!(
			find_secret(
				"https://discord.com/api/webhooks/123/abcdefghijklmnopqrstuvwxyz"
			)
			.is_some()
		);
		assert!(find_secret("https://user:hunter2@example.com/").is_some());
		assert!(
			find_secret(r#"{"apiKey": "1f0c2b3e-7a6d-4b8f-9c1d-2e3f4a5b6c7d"}"#)
				.is_some()
		);
		assert!(find_secret("api_key = \"1f0c2b3e7a6d4b8f9c1d\"").is_some());
		assert!(find_secret("password=hunter").is_some());
		assert!(find_secret(r#"{"clientSecret": "letmein"}"#).is_some());
	}

	#[test]
	fn leaves_ordinary_settings_alone() {
		assert!(
			find_secret(
				"fov:90\nlastServer:play.hypixel.net\nkey_key.attack:key.mouse.left"
			)
			.is_none()
		);
		assert!(
			find_secret(r##"{"tokenColor": "#FF00FF", "showSessionInfo": true}"##)
				.is_none()
		);
		assert!(
			find_secret(r#"{"secretMenuEnabled": "ENABLED", "sessionTimeout": 3000}"#)
				.is_none()
		);
		assert!(
			find_secret(r#"{"keyToken": "key.keyboard.unknown", "password": ""}"#)
				.is_none()
		);
		assert!(find_secret(r#"{"tokenFormat": "dd-MM-yyyy HH:mm"}"#).is_none());
	}

	#[test]
	fn mirrors_the_client_setting_rules() {
		assert!(is_setting_path("config/sodium-options.json"));
		assert!(is_setting_path(
			"oneconfig/profiles/default/polysprint.json"
		));
		assert!(is_setting_path("options.txt"));
		assert!(!is_setting_path("config/hytils/api_key.txt"));
		assert!(!is_setting_path("config/discord-token.json"));
		assert!(!is_setting_path("oneconfig/backup/old.json"));
		assert!(!is_setting_path("config/../options.txt"));
		assert!(!is_setting_path("servers.dat"));
		assert!(!is_setting_path("saves/world/level.dat"));
	}

	#[test]
	fn keeps_a_clean_payload_untouched() {
		let input = scratch("clean.mrpack");
		let output = scratch("clean-out.mrpack");
		write_zip(
			&input,
			&[
				(INDEX_ENTRY, &index_json()),
				(SHARE_ENTRY, &share_json()),
				("overrides/options.txt", "fov:90"),
				("overrides/config/hytils.toml", "showChat = true"),
				("overrides/mods/local.jar", "PK"),
			],
		);

		let result = sanitize(&input, &output, &declared(&input)).expect("test setup");
		assert!(!result.rewritten);
		assert!(result.removed.is_empty());
		std::fs::remove_file(&input).ok();
	}

	#[test]
	fn strips_secrets_and_unsynced_files() {
		let input = scratch("dirty.mrpack");
		let output = scratch("dirty-out.mrpack");
		write_zip(
			&input,
			&[
				(INDEX_ENTRY, &index_json()),
				(SHARE_ENTRY, &share_json()),
				("overrides/options.txt", "fov:90"),
				(
					"overrides/config/hytils.toml",
					"apiKey = \"1f0c2b3e-7a6d-4b8f-9c1d-2e3f4a5b6c7d\"",
				),
				("overrides/config/discord-token.json", "{}"),
				("overrides/servers.dat", "data"),
				("overrides/kubejs/startup.js", "x"),
			],
		);

		let result = sanitize(&input, &output, &declared(&input)).expect("test setup");
		assert!(result.rewritten);
		assert_eq!(
			result.removed,
			vec![
				"config/discord-token.json",
				"config/hytils.toml",
				"kubejs/startup.js",
				"servers.dat",
			]
		);

		let mut archive = ZipArchive::new(File::open(&output).expect("test setup"))
			.expect("test setup");
		let names: Vec<String> = archive.file_names().map(str::to_string).collect();
		assert!(names.contains(&"overrides/options.txt".to_string()));
		assert!(!names.iter().any(|name| name.contains("hytils")));
		let mut share = String::new();
		archive
			.by_name(SHARE_ENTRY)
			.expect("test setup")
			.read_to_string(&mut share)
			.expect("test setup");
		assert!(!share.contains("hytils"));
		assert_eq!(result.sha512, sha512_file(&output).expect("test setup").1);

		std::fs::remove_file(&input).ok();
		std::fs::remove_file(&output).ok();
	}

	#[test]
	fn rejects_mismatched_or_unsafe_payloads() {
		let input = scratch("unsafe.mrpack");
		let output = scratch("unsafe-out.mrpack");
		write_zip(
			&input,
			&[
				(INDEX_ENTRY, &index_json()),
				(SHARE_ENTRY, &share_json()),
				("overrides/../evil.txt", "x"),
			],
		);
		assert!(matches!(
			sanitize(&input, &output, &declared(&input)),
			Err(PayloadError::Invalid(_))
		));

		write_zip(
			&input,
			&[(INDEX_ENTRY, &index_json()), (SHARE_ENTRY, &share_json())],
		);
		let mut wrong = declared(&input);
		wrong.mc_version = "1.20.1".into();
		assert!(matches!(
			sanitize(&input, &output, &wrong),
			Err(PayloadError::Invalid(_))
		));

		let mut tampered = declared(&input);
		tampered.sha512 = "0".repeat(128);
		assert!(matches!(
			sanitize(&input, &output, &tampered),
			Err(PayloadError::Invalid(_))
		));

		std::fs::remove_file(&input).ok();
	}
}
