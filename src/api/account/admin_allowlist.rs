use std::{collections::HashSet, fs, path::Path, str::FromStr};

use uuid::Uuid;

/// f.ex:
///
/// # One Minecraft UUID per line (hyphenated)
/// 123e4567-e89b-12d3-a456-426614174000
/// deadbeef-dead-beef-dead-beefdeadbeef

pub(crate) fn load_admin_allowlist(path: &Path) -> HashSet<Uuid> {
	let Ok(text) = fs::read_to_string(path) else {
		return HashSet::new();
	};

	text.lines()
		.map(str::trim)
		.filter(|l| !l.is_empty() && !l.starts_with('#'))
		.filter_map(|l| Uuid::from_str(l).ok())
		.collect()
}
