use chrono::{DateTime, Utc};

pub(crate) const MAX_ACTIVE_BONUSES: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Requirement {
	Cosmetic(i32),
	Group {
		group_id: i32,
		variant_key: Option<String>,
	},
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EquippedItem {
	pub(crate) cosmetic_id: i32,
	pub(crate) group_id: Option<i32>,
	pub(crate) variant_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetRule {
	pub(crate) id: i32,
	pub(crate) priority: i32,
	pub(crate) active_from: Option<DateTime<Utc>>,
	pub(crate) active_until: Option<DateTime<Utc>>,
	pub(crate) requirements: Vec<Requirement>,
}

impl Requirement {
	pub(crate) fn matches(&self, item: &EquippedItem) -> bool {
		match self {
			Self::Cosmetic(id) => item.cosmetic_id == *id,
			Self::Group {
				group_id,
				variant_key,
			} => {
				item.group_id == Some(*group_id)
					&& variant_key
						.as_ref()
						.is_none_or(|key| item.variant_key.as_ref() == Some(key))
			}
		}
	}
}

impl SetRule {
	pub(crate) fn is_active(&self, now: DateTime<Utc>) -> bool {
		self.active_from.is_none_or(|from| from <= now)
			&& self.active_until.is_none_or(|until| now < until)
	}

	pub(crate) fn matched_items(&self, equipped: &[EquippedItem]) -> Option<Vec<i32>> {
		if self.requirements.is_empty() {
			return None;
		}

		let mut owner_of_item: Vec<Option<usize>> = vec![None; equipped.len()];
		for requirement in 0..self.requirements.len() {
			let mut visited = vec![false; equipped.len()];
			if !self.assign(requirement, equipped, &mut owner_of_item, &mut visited) {
				return None;
			}
		}

		let mut item_of_requirement = vec![0; self.requirements.len()];
		for (item, owner) in owner_of_item.iter().enumerate() {
			if let Some(requirement) = owner {
				item_of_requirement[*requirement] = equipped[item].cosmetic_id;
			}
		}
		Some(item_of_requirement)
	}

	fn assign(
		&self,
		requirement: usize,
		equipped: &[EquippedItem],
		owner_of_item: &mut [Option<usize>],
		visited: &mut [bool],
	) -> bool {
		for (item, candidate) in equipped.iter().enumerate() {
			if visited[item] || !self.requirements[requirement].matches(candidate) {
				continue;
			}
			visited[item] = true;
			let free = match owner_of_item[item] {
				None => true,
				Some(other) => self.assign(other, equipped, owner_of_item, visited),
			};
			if free {
				owner_of_item[item] = Some(requirement);
				return true;
			}
		}
		false
	}
}

pub(crate) fn completed_sets(
	equipped: &[EquippedItem],
	sets: &[SetRule],
	now: DateTime<Utc>,
) -> Vec<i32> {
	let mut complete: Vec<&SetRule> = sets
		.iter()
		.filter(|set| set.is_active(now) && set.matched_items(equipped).is_some())
		.collect();
	complete.sort_by_key(|set| (std::cmp::Reverse(set.priority), set.id));
	complete.into_iter().map(|set| set.id).collect()
}

pub(crate) fn active_of(completed: &[i32]) -> &[i32] {
	&completed[..completed.len().min(MAX_ACTIVE_BONUSES)]
}

pub(crate) fn resolve_bonuses(
	equipped: &[EquippedItem],
	sets: &[SetRule],
	now: DateTime<Utc>,
) -> Vec<i32> {
	active_of(&completed_sets(equipped, sets, now)).to_vec()
}


#[cfg(test)]
mod tests {
	use chrono::{DateTime, Duration, TimeZone, Utc};

	use super::{
		EquippedItem, MAX_ACTIVE_BONUSES, Requirement, SetRule, active_of, completed_sets,
		resolve_bonuses,
	};

	fn now() -> DateTime<Utc> {
		Utc.with_ymd_and_hms(2026, 12, 1, 12, 0, 0).unwrap()
	}

	fn item(cosmetic_id: i32) -> EquippedItem {
		EquippedItem { cosmetic_id, group_id: None, variant_key: None }
	}

	fn grouped(cosmetic_id: i32, group_id: i32, key: Option<&str>) -> EquippedItem {
		EquippedItem { cosmetic_id, group_id: Some(group_id), variant_key: key.map(str::to_owned) }
	}

	fn set(id: i32, priority: i32, requirements: Vec<Requirement>) -> SetRule {
		SetRule { id, priority, active_from: None, active_until: None, requirements }
	}

	fn wear(ids: &[i32]) -> Vec<Requirement> {
		ids.iter().copied().map(Requirement::Cosmetic).collect()
	}

	fn group(group_id: i32, key: Option<&str>) -> Requirement {
		Requirement::Group { group_id, variant_key: key.map(str::to_owned) }
	}

	fn window(id: i32, priority: i32, from: Option<i64>, until: Option<i64>) -> SetRule {
		let at = |days: i64| now() + Duration::days(days);
		SetRule { active_from: from.map(at), active_until: until.map(at), ..set(id, priority, wear(&[10])) }
	}

	#[test]
	fn a_set_needs_every_requirement_and_one_item_per_requirement() {
		let pair = [set(1, 0, wear(&[10, 11]))];
		let any_four = [set(1, 0, vec![group(4, None), Requirement::Cosmetic(20)])];
		let red = [set(1, 0, vec![group(4, Some("red"))])];
		let twice = [set(1, 0, vec![group(4, None), group(4, None)])];
		let overlapping = [set(1, 0, wear(&[10, 11])), set(2, 0, wear(&[11, 12])), set(3, 0, wear(&[12, 13]))];
		let cases: [(&[SetRule], Vec<EquippedItem>, Vec<i32>); 15] = [
			(&pair, vec![item(10), item(11), item(12)], vec![1]),
			(&pair, vec![item(10)], vec![]),
			(&pair, vec![], vec![]),
			(&[set(1, 0, Vec::new())], vec![item(10)], vec![]),
			(&any_four, vec![grouped(40, 4, Some("red")), item(20)], vec![1]),
			(&any_four, vec![grouped(41, 4, Some("blue")), item(20)], vec![1]),
			(&any_four, vec![grouped(50, 5, Some("red")), item(20)], vec![]),
			(&red, vec![grouped(40, 4, Some("red"))], vec![1]),
			(&red, vec![grouped(41, 4, Some("blue"))], vec![]),
			(&red, vec![grouped(42, 5, Some("red"))], vec![]),
			(&red, vec![grouped(43, 4, None)], vec![]),
			(&twice, vec![grouped(40, 4, Some("left"))], vec![]),
			(&twice, vec![grouped(40, 4, Some("left")), grouped(41, 4, Some("right"))], vec![1]),
			(&overlapping, vec![item(10), item(11), item(12)], vec![1, 2]),
			(&overlapping, vec![item(11), item(12), item(13)], vec![2, 3]),
		];
		for (sets, equipped, expected) in cases {
			assert_eq!(resolve_bonuses(&equipped, sets, now()), expected, "{equipped:?}");
		}

		let rule = set(1, 0, vec![group(4, None), group(4, Some("red"))]);
		let equipped = [grouped(40, 4, Some("red")), grouped(41, 4, Some("blue"))];
		assert_eq!(rule.matched_items(&equipped), Some(vec![41, 40]));
	}

	#[test]
	fn the_highest_priority_completed_sets_win_up_to_the_cap() {
		let ties = [set(1, 0, wear(&[10])), set(2, 0, wear(&[10])), set(3, 0, wear(&[10]))];
		let bonuses = resolve_bonuses(&[item(10)], &ties, now());
		assert_eq!((bonuses.len(), bonuses), (MAX_ACTIVE_BONUSES, vec![1, 2]));
		let ranked = [set(5, 1, wear(&[10])), set(2, 1, wear(&[10])), set(1, 0, wear(&[10])), set(9, 7, wear(&[10]))];
		assert_eq!(resolve_bonuses(&[item(10)], &ranked, now()), vec![9, 2]);

		let listed = [set(1, 0, wear(&[10])), set(2, 5, wear(&[10])), set(3, 0, wear(&[10])), set(4, 0, wear(&[11]))];
		let completed = completed_sets(&[item(10)], &listed, now());
		assert_eq!(completed, vec![2, 1, 3]);
		assert_eq!(active_of(&completed), &[2, 1]);
		assert!(active_of(&[]).is_empty());
	}

	#[test]
	fn windows_include_their_start_and_exclude_their_end() {
		let seasonal = [window(1, 0, Some(-1), Some(1))];
		for (days, expected) in [(0, vec![1]), (-2, vec![]), (1, vec![]), (-1, vec![1])] {
			assert_eq!(resolve_bonuses(&[item(10)], &seasonal, now() + Duration::days(days)), expected, "{days}");
		}
		let open_ended = [window(1, 0, Some(0), None), window(2, 0, None, Some(0))];
		assert_eq!(resolve_bonuses(&[item(10)], &open_ended, now() - Duration::seconds(1)), vec![2]);
		assert_eq!(resolve_bonuses(&[item(10)], &open_ended, now()), vec![1]);
		let expired_first = [window(1, 9, None, Some(-1)), set(2, 1, wear(&[10])), set(3, 0, wear(&[10]))];
		assert_eq!(resolve_bonuses(&[item(10)], &expired_first, now()), vec![2, 3]);
	}
}
