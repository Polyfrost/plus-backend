/// Converts a USD major-unit price (e.g. `4.99`) to integer cents.
pub(crate) fn to_cents(base_price: f32) -> i64 {
	(base_price * 100.0).round() as i64
}

#[cfg(test)]
mod tests {
	use super::to_cents;

	#[test]
	fn rounds_to_the_nearest_cent() {
		assert_eq!(to_cents(4.99), 499);
		assert_eq!(to_cents(0.0), 0);
		assert_eq!(to_cents(9.995), 1000);
		assert_eq!(to_cents(3.01), 301);
	}
}
