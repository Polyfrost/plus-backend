/// Derived from the row id, not the name, so a rename cannot orphan the
/// product and a re-run after a crash finds the one that already exists.
pub(crate) fn cosmetic_group_slug(group_id: i32) -> String {
	format!("cosmetic-group-{group_id}")
}

pub(crate) fn cosmetic_slug(cosmetic_id: i32) -> String {
	format!("cosmetic-{cosmetic_id}")
}

pub(crate) fn bundle_slug(bundle_id: i32) -> String {
	format!("bundle-{bundle_id}")
}

pub(crate) fn tag_slug(tag_id: i32) -> String {
	format!("tag-{tag_id}")
}

pub(crate) fn collection_slug(collection_id: i32) -> String {
	format!("collection-{collection_id}")
}

/// PayNow rejects a product whose description is outside this range, and an
/// absent one counts as zero.
const MIN_DESCRIPTION: usize = 25;
const MAX_DESCRIPTION: usize = 50_000;

#[derive(Debug, thiserror::Error)]
#[error("Description must be {MIN_DESCRIPTION} to {MAX_DESCRIPTION} characters long")]
pub(crate) struct DescriptionLength;

/// Absent is allowed: products fill one in, see [`storefront_description`].
pub(crate) fn check_description(description: Option<&str>) -> Result<(), DescriptionLength> {
	match description.map(|description| description.trim().chars().count()) {
		Some(len) if !(MIN_DESCRIPTION..=MAX_DESCRIPTION).contains(&len) => {
			Err(DescriptionLength)
		}
		_ => Ok(()),
	}
}

pub(crate) fn storefront_description(name: &str, description: Option<&str>) -> String {
	let own = description
		.map(str::trim)
		.filter(|description| !description.is_empty());

	let described = match own {
		Some(own) if own.chars().count() >= MIN_DESCRIPTION => own.to_owned(),
		Some(own) => format!("{own} {name}. Part of the OneClient Poly+ store."),
		None => format!("{name}. Part of the OneClient Poly+ store."),
	};

	described.chars().take(MAX_DESCRIPTION).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn checks_description_length() {
		assert!(check_description(None).is_ok());
		assert!(check_description(Some("A flowing cape stitched from starlight.")).is_ok());
		assert!(check_description(Some("A friendly wave.")).is_err());
		assert!(check_description(Some(&"a".repeat(MAX_DESCRIPTION + 1))).is_err());
	}

	#[test]
	fn keeps_a_description_that_is_already_long_enough() {
		let own = "A flowing cape stitched from starlight.";
		assert_eq!(storefront_description("Star Cape", Some(own)), own);
	}

	#[test]
	fn extends_a_short_one_instead_of_discarding_it() {
		let extended = storefront_description("Wave", Some("A friendly wave."));
		assert!(extended.starts_with("A friendly wave."));
		assert!(extended.chars().count() >= MIN_DESCRIPTION);
	}

	#[test]
	fn invents_one_when_there_is_none() {
		for description in [None, Some(""), Some("   ")] {
			let built = storefront_description("Wave", description);
			assert!(built.starts_with("Wave."), "{built}");
			assert!(built.chars().count() >= MIN_DESCRIPTION, "{built}");
		}
	}

	#[test]
	fn an_unnamed_product_still_clears_the_minimum() {
		assert!(storefront_description("", None).chars().count() >= MIN_DESCRIPTION);
	}

	#[test]
	fn truncates_on_a_character_boundary() {
		let long = "é".repeat(MAX_DESCRIPTION + 100);
		let built = storefront_description("Long", Some(&long));
		assert_eq!(built.chars().count(), MAX_DESCRIPTION);
	}
}
