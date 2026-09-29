//! What something costs once the sales and coupons the backend knows about
//! have been applied.
//!
//! The rules live here, in one place, so the storefront, the client mod, the
//! admin dashboard and checkout all quote the same number. What was actually
//! *charged* is a separate fact, reported by whatever settled the payment and
//! recorded verbatim on the transaction.

mod load;
pub(crate) mod display;

pub(crate) use load::{
	LiveDiscount, by_paynow_ids, live_rules, normalise_code, redeem, targets_by_discount,
};
use entities::discount;

/// How much a rule takes off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Amount {
	Percent(i32),
	/// Only ever applied to a line priced in the same currency: "$2 off" means
	/// nothing against a balance denominated in something else.
	Fixed { minor: i64, currency: String },
}

impl Amount {
	/// The amount this takes off `list_minor`, never more than the line costs.
	fn applied_to(&self, list_minor: i64, currency: &str) -> i64 {
		if list_minor <= 0 {
			return 0;
		}

		let off = match self {
			// Rounded to the nearest minor unit, halves away from zero.
			Self::Percent(percent) => {
				(list_minor * i64::from(*percent) + 50) / 100
			}
			Self::Fixed {
				minor,
				currency: own,
			} => {
				if own.eq_ignore_ascii_case(currency) {
					*minor
				} else {
					0
				}
			}
		};

		off.clamp(0, list_minor)
	}
}

/// What a discount covers. Every list is additive: a product matching any one
/// of them is covered.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct Targets {
	#[serde(default)]
	pub collections: Vec<i32>,
	#[serde(default)]
	pub tags: Vec<i32>,
	#[serde(default)]
	pub cosmetics: Vec<i32>,
	#[serde(default)]
	pub cosmetic_groups: Vec<i32>,
	#[serde(default)]
	pub bundles: Vec<i32>,
}

/// One discount, loaded and ready to price with.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
	pub id: i32,
	pub name: String,
	/// `None` for a sale, which applies on its own; `Some` for a coupon, which
	/// only applies when the buyer supplies it.
	pub code: Option<String>,
	pub amount: Amount,
	pub applies_to_all: bool,
	pub targets: Targets,
	pub min_subtotal_minor: Option<i64>,
}

impl Targets {
	pub(crate) fn is_empty(&self) -> bool {
		self.collections.is_empty()
			&& self.tags.is_empty()
			&& self.cosmetics.is_empty()
			&& self.cosmetic_groups.is_empty()
			&& self.bundles.is_empty()
	}
}

impl Rule {
	fn covers(&self, line: &Sellable) -> bool {
		if self.code.is_some() && line.coupons_disabled {
			return false;
		}
		if self.applies_to_all {
			return true;
		}

		let targets = &self.targets;
		line.collection
			.is_some_and(|id| targets.collections.contains(&id))
			|| line.tags.iter().any(|id| targets.tags.contains(id))
			|| line
				.cosmetic_ids
				.iter()
				.any(|id| targets.cosmetics.contains(id))
			|| line
				.cosmetic_group_id
				.is_some_and(|id| targets.cosmetic_groups.contains(&id))
			|| line.bundle_id.is_some_and(|id| targets.bundles.contains(&id))
	}
}

/// One thing being bought, with everything a rule might match on.
#[derive(Debug, Default, Clone)]
pub(crate) struct Sellable {
	pub product_id: String,
	pub list_minor: i64,
	pub collection: Option<i32>,
	pub tags: Vec<i32>,
	pub cosmetic_ids: Vec<i32>,
	pub cosmetic_group_id: Option<i32>,
	pub bundle_id: Option<i32>,
	pub coupons_disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
	pub discount_id: i32,
	pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuotedLine {
	pub product_id: String,
	pub list_minor: i64,
	pub discount_minor: i64,
	pub total_minor: i64,
	pub applied: Vec<Applied>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Quote {
	pub currency: String,
	pub lines: Vec<QuotedLine>,
	pub subtotal_minor: i64,
	pub discount_minor: i64,
	pub total_minor: i64,
}

/// Prices `lines` against every rule that applies, the way PayNow charges
/// them: the best single sale on each line, then every coupon in turn, each
/// on what the last left.
pub(crate) fn quote(lines: &[Sellable], rules: &[Rule], currency: &str) -> Quote {
	let subtotal: i64 = lines.iter().map(|line| line.list_minor).sum();

	// Gated on the list subtotal, so applying a discount cannot drop the
	// basket under its own threshold and turn itself off.
	let eligible: Vec<&Rule> = rules
		.iter()
		.filter(|rule| {
			rule.min_subtotal_minor
				.is_none_or(|minimum| subtotal >= minimum)
		})
		.collect();

	let lines: Vec<QuotedLine> = lines
		.iter()
		.map(|line| {
			let covering = eligible.iter().copied().filter(|rule| rule.covers(line));

			let sale = covering
				.clone()
				.filter(|rule| rule.code.is_none())
				.map(|rule| (rule.amount.applied_to(line.list_minor, currency), rule))
				.filter(|(off, _)| *off > 0)
				// Ties break on the lower id, so a quote is stable rather than
				// depending on what order the rows came back in.
				.max_by(|(a, left), (b, right)| {
					a.cmp(b).then(right.id.cmp(&left.id))
				});

			let mut remaining = line.list_minor;
			let mut applied_rules = Vec::new();
			if let Some((off, rule)) = sale {
				remaining -= off;
				applied_rules.push(rule);
			}

			let mut coupons: Vec<&Rule> =
				covering.filter(|rule| rule.code.is_some()).collect();
			coupons.sort_by_key(|rule| rule.id);
			for rule in coupons {
				let off = rule.amount.applied_to(remaining, currency);
				if off > 0 {
					remaining -= off;
					applied_rules.push(rule);
				}
			}

			let discount_minor = line.list_minor - remaining;
			let applied = applied_rules
				.into_iter()
				.map(|rule| Applied {
					discount_id: rule.id,
					name: rule.name.clone(),
				})
				.collect();

			QuotedLine {
				product_id: line.product_id.clone(),
				list_minor: line.list_minor,
				discount_minor,
				total_minor: line.list_minor - discount_minor,
				applied,
			}
		})
		.collect();

	let discount_minor = lines.iter().map(|line| line.discount_minor).sum();

	Quote {
		currency: currency.to_owned(),
		subtotal_minor: subtotal,
		discount_minor,
		total_minor: subtotal - discount_minor,
		lines,
	}
}

/// The amount a row takes off, or `None` when the row is malformed. The
/// database check constraint makes that unreachable for rows it wrote.
pub(crate) fn amount_of(model: &discount::Model) -> Option<Amount> {
	match (model.percent_off, model.amount_off_minor) {
		(Some(percent), None) => Some(Amount::Percent(percent)),
		(None, Some(minor)) => Some(Amount::Fixed {
			minor,
			currency: model.currency.clone()?,
		}),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sale(id: i32, amount: Amount) -> Rule {
		Rule {
			id,
			name: format!("sale-{id}"),
			code: None,
			amount,
			applies_to_all: true,
			targets: Targets::default(),
			min_subtotal_minor: None,
		}
	}

	fn line(product_id: &str, list_minor: i64) -> Sellable {
		Sellable {
			product_id: product_id.to_owned(),
			list_minor,
			..Default::default()
		}
	}

	#[test]
	fn a_percentage_comes_off_the_list_price() {
		let quoted = quote(&[line("a", 1000)], &[sale(1, Amount::Percent(25))], "usd");
		assert_eq!(quoted.subtotal_minor, 1000);
		assert_eq!(quoted.discount_minor, 250);
		assert_eq!(quoted.total_minor, 750);
	}

	#[test]
	fn a_percentage_rounds_to_the_nearest_minor_unit() {
		// 499 * 10% = 49.9
		let quoted = quote(&[line("a", 499)], &[sale(1, Amount::Percent(10))], "usd");
		assert_eq!(quoted.discount_minor, 50);
	}

	#[test]
	fn the_best_single_discount_wins_rather_than_stacking() {
		let rules = [
			sale(1, Amount::Percent(10)),
			sale(2, Amount::Percent(40)),
			sale(3, Amount::Percent(25)),
		];
		let quoted = quote(&[line("a", 1000)], &rules, "usd");

		assert_eq!(quoted.discount_minor, 400);
		assert_eq!(
			quoted.lines[0].applied.first().map(|a| a.discount_id),
			Some(2)
		);
	}

	#[test]
	fn a_fixed_amount_never_crosses_currencies() {
		let gems = [sale(
			1,
			Amount::Fixed {
				minor: 200,
				currency: "gems".to_owned(),
			},
		)];

		assert_eq!(quote(&[line("a", 1000)], &gems, "gems").discount_minor, 200);
		// The same rule against a dollar-priced line takes nothing off.
		assert_eq!(quote(&[line("a", 1000)], &gems, "usd").discount_minor, 0);
	}

	#[test]
	fn a_discount_never_exceeds_the_line() {
		let rules = [sale(
			1,
			Amount::Fixed {
				minor: 5000,
				currency: "usd".to_owned(),
			},
		)];
		let quoted = quote(&[line("a", 1000)], &rules, "usd");

		assert_eq!(quoted.discount_minor, 1000);
		assert_eq!(quoted.total_minor, 0);
	}

	#[test]
	fn a_rule_only_covers_what_it_targets() {
		let mut winter = sale(1, Amount::Percent(50));
		winter.applies_to_all = false;
		winter.targets.collections = vec![7];

		let mut inside = line("a", 1000);
		inside.collection = Some(7);
		let outside = line("b", 1000);

		let quoted = quote(&[inside, outside], &[winter], "usd");

		assert_eq!(quoted.lines[0].discount_minor, 500);
		assert_eq!(quoted.lines[1].discount_minor, 0);
		assert_eq!(quoted.total_minor, 1500);
	}

	#[test]
	fn a_minimum_is_measured_against_the_list_subtotal() {
		let mut rule = sale(1, Amount::Percent(50));
		rule.min_subtotal_minor = Some(2000);

		assert_eq!(quote(&[line("a", 1000)], &[rule.clone()], "usd").discount_minor, 0);
		// Two lines clear the threshold, and the discount does not then drop
		// the basket back under it.
		assert_eq!(
			quote(&[line("a", 1000), line("b", 1000)], &[rule], "usd").discount_minor,
			1000
		);
	}

	#[test]
	fn coupons_disabled_still_takes_a_sale() {
		let mut line = line("a", 1000);
		line.coupons_disabled = true;
		let mut code = sale(2, Amount::Percent(50));
		code.code = Some("HALF".to_owned());

		let quoted = quote(&[line], &[sale(1, Amount::Percent(10)), code], "usd");
		assert_eq!(quoted.discount_minor, 100);
	}

	#[test]
	fn a_free_line_is_left_alone() {
		let quoted = quote(&[line("a", 0)], &[sale(1, Amount::Percent(50))], "usd");
		assert_eq!(quoted.discount_minor, 0);
		assert!(quoted.lines[0].applied.is_empty());
	}

	#[test]
	fn the_best_sale_then_every_coupon_on_what_is_left() {
		let mut first = sale(3, Amount::Percent(10));
		first.code = Some("TEN".to_owned());
		let mut second = sale(4, Amount::Fixed {
			minor: 100,
			currency: "usd".to_owned(),
		});
		second.code = Some("DOLLAR".to_owned());
		let rules = [
			sale(1, Amount::Percent(20)),
			sale(2, Amount::Percent(5)),
			second,
			first,
		];

		// 1000 - 20% = 800, - 10% = 720, - 100 = 620. The 5% sale loses to
		// the 20% one instead of stacking with it.
		let quoted = quote(&[line("a", 1000)], &rules, "usd");
		assert_eq!(quoted.discount_minor, 380);
		let ids: Vec<i32> = quoted.lines[0].applied.iter().map(|a| a.discount_id).collect();
		assert_eq!(ids, [1, 3, 4]);
	}
}
