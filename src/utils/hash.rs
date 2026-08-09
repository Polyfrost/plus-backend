use sha2::{Digest, Sha256};

/// Hashes `data` and returns the digest as a lowercase hex string.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
	hex(Sha256::digest(data))
}

/// Hashes `parts` as a single digest, separated by a null byte so that
/// different groupings of the same bytes cannot collide.
pub(crate) fn sha256_hex_parts(parts: &[&[u8]]) -> String {
	let mut hasher = Sha256::new();
	for (index, part) in parts.iter().enumerate() {
		if index > 0 {
			hasher.update([0]);
		}
		hasher.update(part);
	}
	hex(hasher.finalize())
}

fn hex(digest: impl AsRef<[u8]>) -> String {
	digest
		.as_ref()
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::{sha256_hex, sha256_hex_parts};

	#[test]
	fn digest_null() {
		assert_eq!(
			sha256_hex(b"null"),
			"74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b"
		)
	}

	#[test]
	fn digest_is_lowercase_hex_sha256() {
		assert_eq!(
			sha256_hex(b"abc"),
			"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
		);
	}

	#[test]
	fn parts_are_separated_so_regroupings_differ() {
		assert_ne!(
			sha256_hex_parts(&[b"ab", b"c"]),
			sha256_hex_parts(&[b"a", b"bc"])
		);
	}
}
