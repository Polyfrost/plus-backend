mod engine;

use std::io::{Cursor, Read};

use serde::Deserialize;
use tokio::task::{JoinError, spawn_blocking};
use zip::ZipArchive;

pub(crate) use self::engine::{
	Engine, EngineError, EngineIssue, IssueLevel, NameplateEngine,
};
#[cfg(feature = "nameplate-engine")]
pub(crate) use self::engine::{engine_issue, skipped};
use crate::{api::ApiState, utils::zip::is_zip};

pub(crate) const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_ENTRIES: usize = 128;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MANIFEST: &str = "manifest.json";
const DEFAULT_VARIANT: &str = "default";
const FORMAT: u64 = 1;

#[derive(thiserror::Error, Debug)]
pub(crate) enum NameplateBundleError {
	#[error("Nameplate bundles must be zip files")]
	NotZip,
	#[error("The bundle is {0} bytes, and clients download at most {MAX_BUNDLE_BYTES}")]
	TooLarge(usize),
	#[error("The bundle has {0} entries, and clients read at most {MAX_ENTRIES}")]
	TooManyEntries(usize),
	#[error(
		"The bundle unpacks to {0} bytes, and clients read at most {MAX_UNPACKED_BYTES}"
	)]
	TooLargeUnpacked(u64),
	#[error(
		"{0} unpacks to {1} bytes, and clients read at most {MAX_FILE_BYTES} per file"
	)]
	FileTooLarge(String, u64),
	#[error("The bundle has no manifest.json")]
	MissingManifest,
	#[error("manifest.json must sit at the root of the bundle, not at {0}")]
	NestedManifest(String),
	#[error("manifest.json is larger than {MAX_MANIFEST_BYTES} bytes")]
	ManifestTooLarge,
	#[error("manifest.json is invalid: {0}")]
	InvalidManifest(#[from] serde_json::Error),
	#[error("manifest.json has format {0}, and only format {FORMAT} is supported")]
	UnsupportedFormat(u64),
	#[error("The bundle has no variant named {0:?}")]
	UnknownVariant(String),
	#[error("The bundle failed validation: {}", describe(.0))]
	Invalid(Vec<EngineIssue>),
	#[error("The bundle validator stopped: {0}")]
	ValidatorFailed(#[from] JoinError),
	#[error("Invalid ZIP bundle: {0}")]
	Zip(#[from] zip::result::ZipError),
	#[error("Unable to read the bundle: {0}")]
	Io(#[from] std::io::Error),
}

#[derive(Deserialize)]
struct Manifest {
	format: u64,
	#[serde(default)]
	variants: Option<serde_json::Map<String, serde_json::Value>>,
}

pub(crate) struct NameplateBundle {
	variants: Vec<String>,
}

pub(crate) fn describe(issues: &[EngineIssue]) -> String {
	issues
		.iter()
		.map(|issue| format!("{} at {}", issue.code, issue.path))
		.collect::<Vec<_>>()
		.join("; ")
}

impl NameplateBundle {
	pub(crate) fn read(data: &[u8]) -> Result<Self, NameplateBundleError> {
		if !is_zip(data) {
			return Err(NameplateBundleError::NotZip);
		}
		if data.len() > MAX_BUNDLE_BYTES {
			return Err(NameplateBundleError::TooLarge(data.len()));
		}

		let mut archive = ZipArchive::new(Cursor::new(data))?;
		if archive.len() > MAX_ENTRIES {
			return Err(NameplateBundleError::TooManyEntries(archive.len()));
		}

		let mut unpacked = 0u64;
		let mut root_manifest = None;
		let mut nested_manifests = Vec::new();
		for index in 0..archive.len() {
			let entry = archive.by_index(index)?;
			unpacked = unpacked.saturating_add(entry.size());
			if entry.is_dir() {
				continue;
			}
			if entry.size() > MAX_FILE_BYTES {
				return Err(NameplateBundleError::FileTooLarge(
					entry.name().to_string(),
					entry.size(),
				));
			}
			match entry.name().split_once('/') {
				None if entry.name() == MANIFEST => root_manifest = Some(index),
				Some((_, rest)) if rest == MANIFEST => nested_manifests.push(index),
				_ => {}
			}
		}
		if unpacked > MAX_UNPACKED_BYTES {
			return Err(NameplateBundleError::TooLargeUnpacked(unpacked));
		}

		let index = match (root_manifest, nested_manifests.as_slice()) {
			(Some(index), _) => index,
			(None, [index]) => {
				return Err(NameplateBundleError::NestedManifest(
					archive
						.name_for_index(*index)
						.unwrap_or(MANIFEST)
						.to_string(),
				));
			}
			_ => return Err(NameplateBundleError::MissingManifest),
		};

		let entry = archive.by_index(index)?;
		let mut text = Vec::new();
		entry.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut text)?;
		if text.len() as u64 > MAX_MANIFEST_BYTES {
			return Err(NameplateBundleError::ManifestTooLarge);
		}

		let manifest: Manifest = serde_json::from_slice(&text)?;
		if manifest.format != FORMAT {
			return Err(NameplateBundleError::UnsupportedFormat(manifest.format));
		}

		Ok(Self {
			variants: match manifest.variants {
				Some(variants) if !variants.is_empty() => {
					variants.into_iter().map(|(key, _)| key).collect()
				}
				_ => vec![DEFAULT_VARIANT.to_string()],
			},
		})
	}

	pub(crate) fn check_variant_key(
		&self,
		key: Option<&str>,
	) -> Result<(), NameplateBundleError> {
		match key {
			Some(key) if !self.variants.iter().any(|variant| variant == key) => {
				Err(NameplateBundleError::UnknownVariant(key.to_string()))
			}
			_ => Ok(()),
		}
	}
}

fn validate_with<E: NameplateEngine>(
	data: &[u8],
) -> Result<Vec<EngineIssue>, NameplateBundleError> {
	let (errors, notes): (Vec<_>, Vec<_>) = E::validate(data)
		.into_iter()
		.partition(|issue| issue.level == IssueLevel::Error);
	if errors.is_empty() {
		Ok(notes)
	} else {
		Err(NameplateBundleError::Invalid(errors))
	}
}

pub(crate) async fn validate_upload(
	data: Vec<u8>,
) -> Result<Vec<EngineIssue>, NameplateBundleError> {
	spawn_blocking(move || validate_with::<Engine>(&data)).await?
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum StoredBundleError {
	#[error(transparent)]
	Bundle(#[from] NameplateBundleError),
	#[error("S3 error: {0}")]
	S3(#[from] s3::error::S3Error),
}

pub(crate) async fn stored_bundle(
	state: &ApiState,
	asset: &entities::asset::Model,
) -> Result<Option<(NameplateBundle, Vec<u8>)>, StoredBundleError> {
	let Some(path) = &asset.storage_path else {
		return Ok(None);
	};
	let data = state.s3_bucket.get_object(path).await?.to_vec();
	Ok(Some((NameplateBundle::read(&data)?, data)))
}

#[cfg(test)]
mod tests {
	#[cfg(not(feature = "nameplate-engine"))]
	use super::validate_upload;
	use super::{
		EngineError, EngineIssue, IssueLevel, MAX_BUNDLE_BYTES, MAX_ENTRIES, MAX_FILE_BYTES,
		NameplateBundle, NameplateBundleError as E, NameplateEngine, engine::Unlinked,
		validate_with,
	};
	use crate::api::v0::cosmetics::engine::tests::{issue, zip};

	struct Linked;

	impl NameplateEngine for Linked {
		fn validate(_: &[u8]) -> Vec<EngineIssue> {
			vec![issue("readability", IssueLevel::Warning)]
		}

		fn cover(_: &[u8], _: Option<&str>) -> Result<Option<Vec<u8>>, EngineError> {
			Ok(None)
		}
	}

	struct Strict;

	impl NameplateEngine for Strict {
		fn validate(_: &[u8]) -> Vec<EngineIssue> {
			vec![
				issue("unknown_key", IssueLevel::Error),
				issue("readability", IssueLevel::Warning),
				issue("flash", IssueLevel::Error),
			]
		}

		fn cover(_: &[u8], _: Option<&str>) -> Result<Option<Vec<u8>>, EngineError> {
			Ok(None)
		}
	}

	fn read(files: &[(&str, &[u8])]) -> Result<NameplateBundle, E> {
		NameplateBundle::read(&zip(files))
	}

	const PLAIN: &[u8] = br#"{"format":1}"#;
	const VARIANTS: &[u8] = br#"{"format":1,"variants":{"blue":{},"red":{}}}"#;

	#[test]
	fn engine_errors_block_the_upload_and_warnings_pass() {
		let error = validate_with::<Strict>(b"bundle").expect_err("errors block");
		let E::Invalid(issues) = &error else {
			panic!("expected validation issues, got {error}");
		};
		let codes: Vec<&str> = issues.iter().map(|issue| issue.code.as_str()).collect();
		assert_eq!(codes, ["unknown_key", "flash"]);
		let warned = validate_with::<Linked>(b"bundle").expect("warnings pass");
		assert_eq!(warned, vec![issue("readability", IssueLevel::Warning)]);
		assert_eq!(validate_with::<Unlinked>(b"bundle").expect("no issues"), Vec::new());
	}

	#[cfg(not(feature = "nameplate-engine"))]
	#[tokio::test]
	async fn uploads_pass_validation_while_the_engine_is_unlinked() {
		assert_eq!(validate_upload(b"bundle".to_vec()).await.unwrap(), Vec::new());
	}

	#[test]
	fn bundles_must_be_small_zips_with_one_root_manifest() {
		assert!(read(&[("manifest.json", PLAIN), ("plate.png", b"png")]).is_ok());
		assert!(matches!(
			read(&[("plate/manifest.json", PLAIN), ("plate/plate.png", b"png")]),
			Err(E::NestedManifest(name)) if name == "plate/manifest.json"
		));
		let root_wins = read(&[("manifest.json", VARIANTS), ("plate/manifest.json", PLAIN)]).unwrap();
		assert!(root_wins.check_variant_key(Some("blue")).is_ok());
		for missing in [&[("plate.png", &b"png"[..])][..], &[("a/manifest.json", PLAIN), ("b/manifest.json", PLAIN)]] {
			assert!(matches!(read(missing), Err(E::MissingManifest)));
		}
		assert!(matches!(read(&[("manifest.json", br#"{"format":2}"#)]), Err(E::UnsupportedFormat(2))));
		for manifest in [&b"{"[..], b"{}"] {
			assert!(matches!(read(&[("manifest.json", manifest)]), Err(E::InvalidManifest(_))));
		}
		assert!(matches!(NameplateBundle::read(b"not a zip"), Err(E::NotZip)));

		let mut data = b"PK\x03\x04".to_vec();
		data.resize(MAX_BUNDLE_BYTES + 1, 0);
		assert!(matches!(NameplateBundle::read(&data), Err(E::TooLarge(size)) if size == MAX_BUNDLE_BYTES + 1));

		let names: Vec<String> = (0..MAX_ENTRIES).map(|i| format!("{i}.png")).collect();
		let mut files: Vec<(&str, &[u8])> = vec![("manifest.json", PLAIN)];
		files.extend(names.iter().take(MAX_ENTRIES - 1).map(|name| (name.as_str(), &b""[..])));
		assert!(read(&files).is_ok());
		files.push((&names[MAX_ENTRIES - 1], b""));
		assert!(matches!(read(&files), Err(E::TooManyEntries(count)) if count == MAX_ENTRIES + 1));

		let exact = vec![0u8; MAX_FILE_BYTES as usize];
		let big = vec![0u8; MAX_FILE_BYTES as usize + 1];
		assert!(read(&[("manifest.json", PLAIN), ("plate.png", &exact)]).is_ok());
		assert!(matches!(
			read(&[("manifest.json", PLAIN), ("plate.png", &big)]),
			Err(E::FileTooLarge(name, size)) if name == "plate.png" && size == MAX_FILE_BYTES + 1
		));
		let mut blocks: Vec<(&str, &[u8])> = names.iter().take(9).map(|name| (name.as_str(), exact.as_slice())).collect();
		blocks.push(("manifest.json", PLAIN));
		assert!(matches!(read(&blocks), Err(E::TooLargeUnpacked(_))));
		let mut manifest = br#"{"format":1,"pad":""#.to_vec();
		manifest.resize(MAX_FILE_BYTES as usize, b'a');
		manifest.extend_from_slice(b"\"}");
		assert!(matches!(read(&[("manifest.json", &manifest)]), Err(E::FileTooLarge(..))));
	}

	#[test]
	fn variant_keys_must_name_a_declared_variant() {
		for manifest in [PLAIN, br#"{"format":1,"variants":{}}"#] {
			let bundle = read(&[("manifest.json", manifest)]).unwrap();
			assert!(bundle.check_variant_key(None).is_ok() && bundle.check_variant_key(Some("default")).is_ok());
			assert!(matches!(bundle.check_variant_key(Some("blue")), Err(E::UnknownVariant(key)) if key == "blue"));
		}
		let bundle = read(&[("manifest.json", VARIANTS)]).unwrap();
		for key in [None, Some("blue"), Some("red")] {
			assert!(bundle.check_variant_key(key).is_ok(), "{key:?}");
		}
		for key in ["default", "green"] {
			assert!(matches!(bundle.check_variant_key(Some(key)), Err(E::UnknownVariant(_))));
		}
	}
}
