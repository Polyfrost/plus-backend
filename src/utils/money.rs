/// Converts a USD major-unit price (e.g. `4.99`) to integer cents.
pub(crate) fn to_cents(base_price: f32) -> i64 {
	(base_price * 100.0).round() as i64
}

/// Applies a percentage discount to a base price, in major units.
pub(crate) fn discounted(base_price: f32, discount_rate: i32) -> f32 {
	base_price * (1.0 - discount_rate as f32 / 100.0)
}

/// What the storefront should actually charge, in minor units.
pub(crate) fn effective_cents(base_price: f32, discount_rate: Option<i32>) -> i64 {
	to_cents(match discount_rate {
		Some(rate) => discounted(base_price, rate),
		None => base_price,
	})
}

#[cfg(test)]
mod tests {
	use super::{effective_cents, to_cents};

	#[test]
	fn rounds_to_the_nearest_cent() {
		assert_eq!(to_cents(4.99), 499);
		assert_eq!(to_cents(0.0), 0);
		assert_eq!(to_cents(9.995), 1000);
		assert_eq!(to_cents(3.01), 301);
	}

	#[test]
	fn applies_a_discount_before_converting() {
		assert_eq!(effective_cents(10.0, None), 1000);
		assert_eq!(effective_cents(10.0, Some(25)), 750);
		assert_eq!(effective_cents(4.99, Some(0)), 499);
	}
}
