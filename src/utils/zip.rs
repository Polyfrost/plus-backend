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
			let mut buf = Vec::with_capacity(entry.size() as usize);
			entry.read_to_end(&mut buf)?;
			writer.write_all(&buf)?;
		}
		writer.finish()?;
	}

	Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
	use super::{is_macos_junk, is_zip};

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
