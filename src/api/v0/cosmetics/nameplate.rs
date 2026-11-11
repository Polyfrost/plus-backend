use std::io::{Cursor, Read};

use serde::Deserialize;
use zip::ZipArchive;

use crate::{api::ApiState, utils::zip::is_zip};

const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
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
