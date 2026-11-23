use std::io::{Cursor, Read, Write};

use zip::result::ZipError;

/// Whether `data` starts with the zip local file header magic.
pub(crate) fn is_zip(data: &[u8]) -> bool {
	data.len() >= 4 && &data[0..4] == b"PK\x03\x04"
}

/// Entries macOS adds when compressing from Finder, which are meaningless to
/// every other platform.
fn is_macos_junk(name: &str) -> bool {
	name.split('/')
		.next_back()
		.is_some_and(|base| base == ".DS_Store")
		|| name.starts_with("__MACOSX/")
		|| name.contains("/__MACOSX/")
}

/// Rewrites `data` as a new archive without directory entries or macOS junk.
pub(crate) fn strip_macos_junk(data: &[u8]) -> Result<Vec<u8>, ZipError> {
	strip_macos_junk_capped(data, u64::MAX)
}

pub(crate) fn strip_macos_junk_capped(
	data: &[u8],
	max_unpacked: u64,
) -> Result<Vec<u8>, ZipError> {
	let mut remaining = max_unpacked;
	let mut archive = zip::ZipArchive::new(Cursor::new(data))?;
	let mut out = Cursor::new(Vec::new());
	{
		let mut writer = zip::ZipWriter::new(&mut out);
		for i in 0..archive.len() {
			let mut entry = archive.by_index(i)?;
			let name = entry.name().to_string();
			if entry.is_dir() || is_macos_junk(&name) {
				continue;
			}
			let options = zip::write::SimpleFileOptions::default()
				.compression_method(zip::CompressionMethod::Deflated);
			writer.start_file(name, options)?;
			let mut buf = Vec::with_capacity(entry.size().min(remaining) as usize);
			(&mut entry)
				.take(remaining.saturating_add(1))
				.read_to_end(&mut buf)?;
			remaining = remaining.checked_sub(buf.len() as u64).ok_or(
				ZipError::InvalidArchive("The archive unpacks past its size limit"),
			)?;
			writer.write_all(&buf)?;
		}
		writer.finish()?;
	}

	Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
	use std::io::{Cursor, Read, Write};

	use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

	use super::{is_macos_junk, is_zip, strip_macos_junk_capped};

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

	fn names(data: &[u8]) -> Vec<String> {
		let archive = ZipArchive::new(Cursor::new(data)).expect("read the stripped zip");
		archive.file_names().map(str::to_owned).collect()
	}

	#[test]
	fn capped_strip_rejects_a_zip_bomb() {
		let bomb = zip(&[("bomb.bin", &vec![0u8; 16 * 1024 * 1024])]);
		assert!(bomb.len() < 64 * 1024);
		assert!(strip_macos_junk_capped(&bomb, 1024 * 1024).is_err());
	}

	#[test]
	fn capped_strip_counts_every_entry_towards_the_cap() {
		let block = vec![0u8; 600 * 1024];
		let data = zip(&[("a.bin", &block), ("b.bin", &block)]);
		assert!(strip_macos_junk_capped(&data, 1024 * 1024).is_err());
		assert!(strip_macos_junk_capped(&data, 2 * 600 * 1024).is_ok());
	}

	#[test]
	fn capped_strip_keeps_content_and_drops_junk() {
		let data = zip(&[
			("plate/manifest.json", b"{}"),
			("__MACOSX/plate/._manifest.json", b"junk"),
			("plate/.DS_Store", b"junk"),
		]);
		let stripped = strip_macos_junk_capped(&data, 1024).expect("strip a small zip");
		assert_eq!(names(&stripped), vec!["plate/manifest.json".to_string()]);

		let mut archive = ZipArchive::new(Cursor::new(stripped)).expect("read the stripped zip");
		let mut text = String::new();
		archive
			.by_name("plate/manifest.json")
			.expect("the manifest survives")
			.read_to_string(&mut text)
			.expect("read the manifest");
		assert_eq!(text, "{}");
	}

	#[test]
	fn detects_zip_magic() {
		assert!(is_zip(b"PK\x03\x04rest"));
		assert!(!is_zip(b"\x89PNG"));
		assert!(!is_zip(b"PK"));
	}

	#[test]
	fn detects_macos_junk() {
		assert!(is_macos_junk("__MACOSX/cape.png"));
		assert!(is_macos_junk("bundle/__MACOSX/cape.png"));
		assert!(is_macos_junk(".DS_Store"));
		assert!(is_macos_junk("bundle/.DS_Store"));
		assert!(!is_macos_junk("bundle/cape.png"));
	}
}
