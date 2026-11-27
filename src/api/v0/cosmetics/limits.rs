use std::io::Cursor;

use entities::sea_orm_active_enums::CosmeticType;
use tokio::task::{JoinError, spawn_blocking};
use zip::{ZipArchive, result::ZipError};

use super::nameplate;
use crate::utils::zip::{is_macos_junk, strip_macos_junk_capped};

const MIB: usize = 1024 * 1024;
const MULTIPART_OVERHEAD: usize = 64 * 1024;
pub(crate) const MAX_COVER_BYTES: usize = 2 * MIB;
pub(crate) const SET_COVER_BODY_LIMIT: usize = MAX_COVER_BYTES + MULTIPART_OVERHEAD;
pub(crate) const REPLACE_ASSET_BODY_LIMIT: usize =
	BundleLimits::largest_download() + MULTIPART_OVERHEAD;
pub(crate) const CREATE_BODY_LIMIT: usize = REPLACE_ASSET_BODY_LIMIT + MAX_COVER_BYTES;
pub(crate) const SET_EFFECT_BODY_LIMIT: usize =
	BundleLimits::EFFECT.download + MULTIPART_OVERHEAD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BundleLimits {
	pub(crate) download: usize,
	pub(crate) unpacked: u64,
	pub(crate) entries: usize,
}

impl BundleLimits {
	pub(crate) const CAPE: Self = Self {
		download: 16 * MIB,
		unpacked: 32 * MIB as u64,
		entries: 16,
	};
	pub(crate) const ITEM: Self = Self {
		download: 8 * MIB,
		unpacked: 16 * MIB as u64,
		entries: 64,
	};
	pub(crate) const EMOTE: Self = Self {
		download: 8 * MIB,
		unpacked: 16 * MIB as u64,
		entries: 128,
	};
	pub(crate) const EFFECT: Self = Self {
		download: 4 * MIB,
		unpacked: 8 * MIB as u64,
		entries: 128,
	};
	pub(crate) const NAMEPLATE: Self = Self {
		download: nameplate::MAX_BUNDLE_BYTES,
		unpacked: nameplate::MAX_UNPACKED_BYTES,
		entries: nameplate::MAX_ENTRIES,
	};
	const ALL: [Self; 5] =
		[Self::CAPE, Self::ITEM, Self::EMOTE, Self::EFFECT, Self::NAMEPLATE];

	pub(crate) fn of(cosmetic_type: &CosmeticType) -> Self {
		match cosmetic_type {
			CosmeticType::Cape => Self::CAPE,
			CosmeticType::Emote => Self::EMOTE,
			CosmeticType::Effect => Self::EFFECT,
			CosmeticType::Nameplate => Self::NAMEPLATE,
			CosmeticType::Backpack
			| CosmeticType::Glasses
			| CosmeticType::Wings
			| CosmeticType::Glove
			| CosmeticType::Hat
			| CosmeticType::Aura
			| CosmeticType::Boots
			| CosmeticType::Shoulder
			| CosmeticType::Pet => Self::ITEM,
		}
	}

	const fn largest_download() -> usize {
		let mut largest = 0;
		let mut index = 0;
		while index < Self::ALL.len() {
			if Self::ALL[index].download > largest {
				largest = Self::ALL[index].download;
			}
			index += 1;
		}
		largest
	}
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum BundleLimitError {
	#[error("The file is {size} bytes, and clients download at most {max}")]
	TooLarge { size: usize, max: usize },
	#[error("The bundle has {count} entries, and clients read at most {max}")]
	TooManyEntries { count: usize, max: usize },
	#[error("The bundle unpacks to {size} bytes, and clients read at most {max}")]
	TooLargeUnpacked { size: u64, max: u64 },
	#[error("Invalid ZIP bundle: {0}")]
	Zip(#[from] ZipError),
	#[error("The bundle check stopped: {0}")]
	Stopped(#[from] JoinError),
}

pub(crate) fn check_download(
	size: usize,
	limits: BundleLimits,
) -> Result<(), BundleLimitError> {
	if size > limits.download {
		return Err(BundleLimitError::TooLarge {
			size,
			max: limits.download,
		});
	}
	Ok(())
}

pub(crate) fn prepare_bundle(
	data: &[u8],
	limits: BundleLimits,
) -> Result<Vec<u8>, BundleLimitError> {
	check_download(data.len(), limits)?;

	let mut archive = ZipArchive::new(Cursor::new(data))?;
	let mut count = 0usize;
	let mut unpacked = 0u64;
	for index in 0..archive.len() {
		let entry = archive.by_index_raw(index)?;
		if entry.is_dir() || is_macos_junk(entry.name()) {
			continue;
		}
		count += 1;
		unpacked = unpacked.saturating_add(entry.size());
	}
	if count > limits.entries {
		return Err(BundleLimitError::TooManyEntries {
			count,
			max: limits.entries,
		});
	}
	if unpacked > limits.unpacked {
		return Err(BundleLimitError::TooLargeUnpacked {
			size: unpacked,
			max: limits.unpacked,
		});
	}

	let stripped = strip_macos_junk_capped(data, limits.unpacked)?;
	check_download(stripped.len(), limits)?;
	Ok(stripped)
}

pub(crate) async fn prepare_bundle_off_thread(
	data: impl AsRef<[u8]> + Send + 'static,
	limits: BundleLimits,
) -> Result<Vec<u8>, BundleLimitError> {
	spawn_blocking(move || prepare_bundle(data.as_ref(), limits)).await?
}

#[cfg(test)]
mod tests {
	use std::io::{Cursor, Write};

	use entities::sea_orm_active_enums::CosmeticType;
	use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

	use super::{
		BundleLimitError, BundleLimits, CREATE_BODY_LIMIT, MAX_COVER_BYTES,
		REPLACE_ASSET_BODY_LIMIT, SET_COVER_BODY_LIMIT, SET_EFFECT_BODY_LIMIT, check_download,
		prepare_bundle, prepare_bundle_off_thread,
	};

	fn zip<N: AsRef<str>, D: AsRef<[u8]>>(files: impl IntoIterator<Item = (N, D)>) -> Vec<u8> {
		let mut out = Cursor::new(Vec::new());
		let mut writer = ZipWriter::new(&mut out);
		let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
		for (name, data) in files {
			writer.start_file(name.as_ref(), options).expect("start a test entry");
			writer.write_all(data.as_ref()).expect("write a test entry");
		}
		writer.finish().expect("finish the test zip");
		out.into_inner()
	}

	fn numbered(count: usize) -> Vec<(String, Vec<u8>)> {
		(0..count).map(|i| (format!("{i}.png"), Vec::new())).collect()
	}

	#[test]
	fn every_type_has_the_client_limits_and_the_bodies_fit_them() {
		use CosmeticType as T;
		let own = [
			(T::Cape, BundleLimits::CAPE),
			(T::Emote, BundleLimits::EMOTE),
			(T::Effect, BundleLimits::EFFECT),
			(T::Nameplate, BundleLimits::NAMEPLATE),
		];
		for (kind, limits) in own {
			assert_eq!(BundleLimits::of(&kind), limits);
		}
		for item in [T::Wings, T::Hat, T::Pet] {
			assert_eq!(BundleLimits::of(&item), BundleLimits::ITEM, "{item:?}");
		}

		let axum_default = 2 * 1024 * 1024;
		for limits in BundleLimits::ALL {
			assert!(REPLACE_ASSET_BODY_LIMIT > limits.download);
			assert!(CREATE_BODY_LIMIT > limits.download + MAX_COVER_BYTES);
		}
		assert_eq!(BundleLimits::largest_download(), BundleLimits::CAPE.download);
		assert!(SET_EFFECT_BODY_LIMIT > BundleLimits::EFFECT.download && SET_EFFECT_BODY_LIMIT > axum_default);
		assert!(SET_EFFECT_BODY_LIMIT < REPLACE_ASSET_BODY_LIMIT);
		assert!(SET_COVER_BODY_LIMIT > MAX_COVER_BYTES && SET_COVER_BODY_LIMIT > axum_default);
	}

	#[test]
	fn bundles_are_checked_against_every_cap() {
		let junk = zip([("manifest.json", &b"{}"[..]), ("__MACOSX/._manifest.json", b"junk")]);
		let prepared = prepare_bundle(&junk, BundleLimits::EFFECT).unwrap();
		let archive = ZipArchive::new(Cursor::new(prepared)).unwrap();
		assert_eq!(archive.file_names().collect::<Vec<_>>(), ["manifest.json"]);
		assert!(matches!(prepare_bundle(b"\x89PNG", BundleLimits::EFFECT), Err(BundleLimitError::Zip(_))));

		let limits = BundleLimits::EFFECT;
		let mut data = zip([("manifest.json", b"{}")]);
		data.resize(limits.download + 1, 0);
		assert!(matches!(
			prepare_bundle(&data, limits),
			Err(BundleLimitError::TooLarge { size, max }) if size == limits.download + 1 && max == limits.download
		));
		assert!(check_download(limits.download, limits).is_ok());
		assert!(check_download(limits.download + 1, limits).is_err());

		for limits in [BundleLimits::EFFECT, BundleLimits::ITEM, BundleLimits::CAPE] {
			assert!(matches!(
				prepare_bundle(&zip(numbered(limits.entries + 1)), limits),
				Err(BundleLimitError::TooManyEntries { count, max })
					if count == limits.entries + 1 && max == limits.entries
			));
			let mut files = numbered(limits.entries);
			files.push(("__MACOSX/._0.png".to_string(), b"junk".to_vec()));
			files.push((".DS_Store".to_string(), b"junk".to_vec()));
			assert!(prepare_bundle(&zip(files), limits).is_ok());
		}

		let half = vec![0u8; limits.unpacked as usize / 2];
		let over = zip([("a.bin", half.as_slice()), ("b.bin", &half), ("c.bin", b"x")]);
		assert!(over.len() < limits.download);
		assert!(matches!(
			prepare_bundle(&over, limits),
			Err(BundleLimitError::TooLargeUnpacked { size, .. }) if size == limits.unpacked + 1
		));
		assert!(prepare_bundle(&zip([("a.bin", &half), ("b.bin", &half)]), limits).is_ok());

		let emote = BundleLimits::EMOTE;
		let bomb = zip([("bomb.bin", vec![0u8; emote.unpacked as usize + 1])]);
		assert!(bomb.len() < 64 * 1024);
		assert!(matches!(prepare_bundle(&bomb, emote), Err(BundleLimitError::TooLargeUnpacked { .. })));
	}

	#[tokio::test]
	async fn off_thread_preparation_keeps_the_caps() {
		let limits = BundleLimits::EFFECT;
		let half = vec![0u8; limits.unpacked as usize / 2];
		let bomb = zip([("a.bin", half.as_slice()), ("b.bin", &half), ("c.bin", b"x")]);
		let prepared = prepare_bundle_off_thread(bomb, limits).await;
		assert!(matches!(prepared, Err(BundleLimitError::TooLargeUnpacked { .. })));
		assert!(prepare_bundle_off_thread(zip([("manifest.json", b"{}")]), limits).await.is_ok());
	}
}
