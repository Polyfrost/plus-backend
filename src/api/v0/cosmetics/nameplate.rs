use std::io::{Cursor, Read};

use serde::Deserialize;
use zip::ZipArchive;

use crate::{api::ApiState, utils::zip::is_zip};

const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
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
	#[error("{0} unpacks to {1} bytes, and clients read at most {MAX_FILE_BYTES} per file")]
	FileTooLarge(String, u64),
	#[error("The bundle has no manifest.json")]
	MissingManifest,
	#[error("manifest.json is larger than {MAX_MANIFEST_BYTES} bytes")]
	ManifestTooLarge,
	#[error("manifest.json is invalid: {0}")]
	InvalidManifest(#[from] serde_json::Error),
	#[error("manifest.json has format {0}, and only format {FORMAT} is supported")]
	UnsupportedFormat(u64),
	#[error("The bundle has no variant named {0:?}")]
	UnknownVariant(String),
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
			(None, [index]) => *index,
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
) -> Result<Option<NameplateBundle>, StoredBundleError> {
	let Some(path) = &asset.storage_path else {
		return Ok(None);
	};
	let data = state.s3_bucket.get_object(path).await?.to_vec();
	Ok(Some(NameplateBundle::read(&data)?))
}

#[cfg(test)]
mod tests {
	use std::io::{Cursor, Write};

	use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

	use super::{
		MAX_BUNDLE_BYTES, MAX_ENTRIES, MAX_FILE_BYTES, NameplateBundle, NameplateBundleError,
	};

	fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
		let mut out = Cursor::new(Vec::new());
		let mut writer = ZipWriter::new(&mut out);
		let options =
			SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
		for (name, data) in files {
			writer.start_file(*name, options).expect("start a test entry");
			writer.write_all(data).expect("write a test entry");
		}
		writer.finish().expect("finish the test zip");
		out.into_inner()
	}

	fn read(files: &[(&str, &[u8])]) -> Result<NameplateBundle, NameplateBundleError> {
		NameplateBundle::read(&zip(files))
	}

	const PLAIN: &[u8] = br#"{"format":1}"#;
	const VARIANTS: &[u8] = br#"{"format":1,"variants":{"blue":{},"red":{}}}"#;

	#[test]
	fn reads_a_plain_bundle() {
		assert!(read(&[("manifest.json", PLAIN), ("plate.png", b"png")]).is_ok());
	}

	#[test]
	fn reads_a_manifest_inside_one_folder() {
		assert!(read(&[("plate/manifest.json", PLAIN)]).is_ok());
	}

	#[test]
	fn rejects_data_that_is_not_a_zip() {
		assert!(matches!(
			NameplateBundle::read(b"not a zip"),
			Err(NameplateBundleError::NotZip)
		));
	}

	#[test]
	fn rejects_an_oversized_zip() {
		let mut data = b"PK\x03\x04".to_vec();
		data.resize(MAX_BUNDLE_BYTES + 1, 0);
		assert!(matches!(
			NameplateBundle::read(&data),
			Err(NameplateBundleError::TooLarge(size)) if size == MAX_BUNDLE_BYTES + 1
		));
	}

	#[test]
	fn rejects_too_many_entries() {
		let names: Vec<String> = (0..MAX_ENTRIES).map(|i| format!("{i}.png")).collect();
		let mut files: Vec<(&str, &[u8])> =
			names.iter().map(|name| (name.as_str(), &b""[..])).collect();
		files.push(("manifest.json", PLAIN));
		assert!(matches!(
			read(&files),
			Err(NameplateBundleError::TooManyEntries(count)) if count == MAX_ENTRIES + 1
		));
	}

	#[test]
	fn accepts_the_entry_limit() {
		let names: Vec<String> =
			(0..MAX_ENTRIES - 1).map(|i| format!("{i}.png")).collect();
		let mut files: Vec<(&str, &[u8])> =
			names.iter().map(|name| (name.as_str(), &b""[..])).collect();
		files.push(("manifest.json", PLAIN));
		assert!(read(&files).is_ok());
	}

	#[test]
	fn rejects_a_large_unpacked_size() {
		let block = vec![0u8; MAX_FILE_BYTES as usize];
		let names: Vec<String> = (0..9).map(|i| format!("{i}.png")).collect();
		let mut files: Vec<(&str, &[u8])> =
			names.iter().map(|name| (name.as_str(), block.as_slice())).collect();
		files.push(("manifest.json", PLAIN));
		assert!(matches!(
			read(&files),
			Err(NameplateBundleError::TooLargeUnpacked(_))
		));
	}

	#[test]
	fn rejects_a_file_over_the_per_file_cap() {
		let big = vec![0u8; MAX_FILE_BYTES as usize + 1];
		assert!(matches!(
			read(&[("manifest.json", PLAIN), ("plate.png", &big)]),
			Err(NameplateBundleError::FileTooLarge(name, size))
				if name == "plate.png" && size == MAX_FILE_BYTES + 1
		));
	}

	#[test]
	fn accepts_a_file_at_the_per_file_cap() {
		let exact = vec![0u8; MAX_FILE_BYTES as usize];
		assert!(read(&[("manifest.json", PLAIN), ("plate.png", &exact)]).is_ok());
	}

	#[test]
	fn rejects_a_missing_manifest() {
		assert!(matches!(
			read(&[("plate.png", b"png")]),
			Err(NameplateBundleError::MissingManifest)
		));
		assert!(matches!(
			read(&[("a/manifest.json", PLAIN), ("b/manifest.json", PLAIN)]),
			Err(NameplateBundleError::MissingManifest)
		));
	}

	#[test]
	fn rejects_an_unsupported_format() {
		assert!(matches!(
			read(&[("manifest.json", br#"{"format":2}"#)]),
			Err(NameplateBundleError::UnsupportedFormat(2))
		));
	}

	#[test]
	fn rejects_an_invalid_manifest() {
		assert!(matches!(
			read(&[("manifest.json", b"{")]),
			Err(NameplateBundleError::InvalidManifest(_))
		));
		assert!(matches!(
			read(&[("manifest.json", b"{}")]),
			Err(NameplateBundleError::InvalidManifest(_))
		));
	}

	#[test]
	fn rejects_an_oversized_manifest() {
		let mut manifest = br#"{"format":1,"pad":""#.to_vec();
		manifest.resize(MAX_FILE_BYTES as usize, b'a');
		manifest.extend_from_slice(b"\"}");
		assert!(matches!(
			read(&[("manifest.json", &manifest)]),
			Err(NameplateBundleError::FileTooLarge(..))
		));
	}

	#[test]
	fn a_bundle_without_variants_only_has_the_default_key() {
		let bundle = read(&[("manifest.json", PLAIN)]).expect("a valid bundle");
		assert!(bundle.check_variant_key(None).is_ok());
		assert!(bundle.check_variant_key(Some("default")).is_ok());
		assert!(matches!(
			bundle.check_variant_key(Some("blue")),
			Err(NameplateBundleError::UnknownVariant(key)) if key == "blue"
		));
	}

	#[test]
	fn empty_variants_count_as_none() {
		let bundle = read(&[("manifest.json", br#"{"format":1,"variants":{}}"#)]).expect("a valid bundle");
		assert!(bundle.check_variant_key(Some("default")).is_ok());
	}

	#[test]
	fn a_bundle_with_variants_checks_its_keys() {
		let bundle = read(&[("manifest.json", VARIANTS)]).expect("a valid bundle");
		assert!(bundle.check_variant_key(None).is_ok());
		assert!(bundle.check_variant_key(Some("blue")).is_ok());
		assert!(bundle.check_variant_key(Some("red")).is_ok());
		assert!(matches!(
			bundle.check_variant_key(Some("default")),
			Err(NameplateBundleError::UnknownVariant(_))
		));
		assert!(matches!(
			bundle.check_variant_key(Some("green")),
			Err(NameplateBundleError::UnknownVariant(_))
		));
	}
}
