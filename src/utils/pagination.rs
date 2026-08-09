/// The largest page a client may ask for, whatever `nb` it sends.
pub(crate) const MAX_PAGE_SIZE: u64 = 100;

/// Serde default for the page size (`nb`).
pub(crate) fn default_page_size() -> u64 {
	50
}

/// Serde default for the 1-indexed page number.
pub(crate) fn default_page() -> u64 {
	1
}
