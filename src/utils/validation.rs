/// The longest slug accepted in a url path segment.
const MAX_SLUG_LEN: usize = 128;
/// The longest url accepted as a redirect target.
const MAX_URL_LEN: usize = 2048;

/// Whether `slug` is a safe url path segment: non-empty, lowercase ascii
/// alphanumerics plus `-` and `_`.
pub(crate) fn valid_slug(slug: &str) -> bool {
	!slug.is_empty()
		&& slug.len() <= MAX_SLUG_LEN
		&& slug
			.chars()
			.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Whether `url` is an absolute http(s) url of a sane length. This rejects
/// relative paths and scheme-based payloads such as `javascript:` or `data:`.
pub(crate) fn valid_http_url(url: &str) -> bool {
	(url.starts_with("https://") || url.starts_with("http://"))
		&& url.len() <= MAX_URL_LEN
}

#[cfg(test)]
mod tests {
	use super::{valid_http_url, valid_slug};

	#[test]
	fn accepts_reasonable_slugs() {
		assert!(valid_slug("oneclient"));
		assert!(valid_slug("oneclient-twitter"));
		assert!(valid_slug("promo_2026"));
	}

	#[test]
	fn rejects_bad_slugs() {
		assert!(!valid_slug(""));
		assert!(!valid_slug("Has Space"));
		assert!(!valid_slug("UPPER"));
		assert!(!valid_slug("emoji-\u{1f600}"));
		assert!(!valid_slug(&"x".repeat(129)));
	}

	#[test]
	fn only_absolute_http_urls_allowed() {
		assert!(valid_http_url("https://polyfrost.org/projects/oneclient"));
		assert!(valid_http_url("http://example.com"));
		assert!(!valid_http_url("/projects/oneclient"));
		assert!(!valid_http_url("javascript:alert(1)"));
		assert!(!valid_http_url("data:text/html,x"));
		assert!(!valid_http_url(&format!(
			"https://x.com/{}",
			"a".repeat(2048)
		)));
	}
}
