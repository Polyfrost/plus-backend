use std::collections::HashMap;

use entities::transaction_line;

use crate::paynow::models::Order;

/// Bail out rather than explore an unbounded number of subset sums.
const MAX_SUBSET_STATES: usize = 4096;

#[derive(Debug)]
pub(super) enum Attribution {
	Full,
	Lines(Vec<i64>),
	/// Not tied to any one set of lines. Revoking the wrong player's cosmetic
	/// is worse than leaving it to a human.
	Undecidable,
}

/// `outstanding` is the lines not yet refunded.
pub(super) fn attribute(
	order: Option<&Order>,
	outstanding: &[transaction_line::Model],
	amount_minor: i64,
) -> Attribution {
	if outstanding.is_empty() {
		return Attribution::Undecidable;
	}

	if let Some(order) = order
		&& let Some(lines) = from_reported_lines(order, outstanding)
	{
		return if lines.len() == outstanding.len() {
			Attribution::Full
		} else {
			Attribution::Lines(lines)
		};
	}

	let total: i64 = outstanding.iter().map(|line| line.total_minor).sum();
	if amount_minor >= total {
		return Attribution::Full;
	}

	match unique_subset(outstanding, amount_minor) {
		Some(lines) => Attribution::Lines(lines),
		None => Attribution::Undecidable,
	}
}

/// Preferred path: the order says which lines were refunded.
fn from_reported_lines(
	order: &Order,
	outstanding: &[transaction_line::Model],
) -> Option<Vec<i64>> {
	let by_provider_id: HashMap<&str, i64> = outstanding
		.iter()
		.map(|line| (line.provider_line_id.as_str(), line.id))
		.collect();

	let mut reported = false;
	let mut refunded = Vec::new();
	for line in &order.lines {
		let is_refunded = match (line.refunded_amount, line.refund_status.as_deref()) {
			(Some(amount), _) => {
				reported = true;
				amount > 0
			}
			(None, Some(status)) => {
				reported = true;
				status != "none" && status != "unknown"
			}
			(None, None) => continue,
		};

		if is_refunded && let Some(id) = by_provider_id.get(line.id.as_str()) {
			refunded.push(*id);
		}
	}

	(reported && !refunded.is_empty()).then_some(refunded)
}

/// The only combination summing to `target`, or nothing: two baskets costing
/// the same are indistinguishable from the amount alone.
fn unique_subset(
	outstanding: &[transaction_line::Model],
	target: i64,
) -> Option<Vec<i64>> {
	if target <= 0 {
		return None;
	}

	// sum -> (how many distinct subsets reach it, one of them)
	let mut reachable: HashMap<i64, (u8, Vec<i64>)> = HashMap::new();
	reachable.insert(0, (1, Vec::new()));

	for line in outstanding {
		if line.total_minor <= 0 {
			// Indistinguishable from not refunding it at all.
			return None;
		}

		let mut next = reachable.clone();
		for (sum, (count, witness)) in &reachable {
			let candidate = sum + line.total_minor;
			if candidate > target {
				continue;
			}

			let mut extended = witness.clone();
			extended.push(line.id);

			let entry = next.entry(candidate).or_insert((0, extended));
			entry.0 = entry.0.saturating_add(*count);
		}

		if next.len() > MAX_SUBSET_STATES {
			return None;
		}
		reachable = next;
	}

	match reachable.remove(&target) {
		Some((1, lines)) => Some(lines),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use chrono::Utc;
	use entities::sea_orm_active_enums::TransactionStatus;

	use super::*;

	fn line(id: i64, total_minor: i64) -> transaction_line::Model {
		transaction_line::Model {
			id,
			transaction_id: 1,
			provider_line_id: format!("line-{id}"),
			product_id: format!("product-{id}"),
			bundle_id: None,
			cosmetic_group_id: None,
			cosmetic_id: None,
			recipient_id: None,
			quantity: 1,
			price_minor: total_minor,
			discount_minor: 0,
			subtotal_minor: total_minor,
			tax_minor: 0,
			total_minor,
			currency: "usd".to_string(),
			status: TransactionStatus::Completed,
			returned_minor: 0,
			returned_at: None,
			created_at: Utc::now().into(),
		}
	}

	#[test]
	fn a_refund_covering_everything_is_full() {
		let lines = [line(1, 500), line(2, 700)];
		assert!(matches!(attribute(None, &lines, 1200), Attribution::Full));
	}

	#[test]
	fn a_uniquely_identifiable_subset_is_attributed() {
		let lines = [line(1, 500), line(2, 700), line(3, 900)];
		let Attribution::Lines(attributed) = attribute(None, &lines, 900) else {
			panic!("expected the 900 line to be identified");
		};
		assert_eq!(attributed, vec![3]);
	}

	#[test]
	fn an_ambiguous_amount_is_left_alone() {
		let lines = [line(1, 500), line(2, 500)];
		assert!(matches!(
			attribute(None, &lines, 500),
			Attribution::Undecidable
		));
	}

	#[test]
	fn an_amount_matching_no_subset_is_left_alone() {
		let lines = [line(1, 500), line(2, 700)];
		assert!(matches!(
			attribute(None, &lines, 600),
			Attribution::Undecidable
		));
	}

	#[test]
	fn a_free_line_makes_a_partial_refund_ambiguous() {
		// 700 could be the paid line alone or the paid line plus the free one.
		let lines = [line(1, 0), line(2, 700), line(3, 900)];
		assert!(matches!(
			attribute(None, &lines, 700),
			Attribution::Undecidable
		));
	}

	#[test]
	fn covering_every_outstanding_line_is_still_full() {
		let lines = [line(1, 0), line(2, 700)];
		assert!(matches!(attribute(None, &lines, 700), Attribution::Full));
	}
}
