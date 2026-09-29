//! What a player owns, worked out from every grant they hold.
//!
//! Each purchase or admin grant is one `ownership_grant` row: for good, or for
//! some days. `player_owned_cosmetic` is kept equal to what the active grants
//! add up to, so refunding one purchase takes back exactly what it bought.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, Utc};
use entities::{
	ownership_grant, player_owned_cosmetic, prelude::*,
	sea_orm_active_enums::TransactionProvider,
};
use sea_orm::{ActiveValue, DbErr, QueryOrder as _, Set, prelude::*};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
	Permanent,
	Until(DateTimeWithTimeZone),
	Gone,
}

impl Held {
	pub(crate) fn expires_at(self) -> Option<DateTimeWithTimeZone> {
		match self {
			Self::Until(at) => Some(at),
			Self::Permanent | Self::Gone => None,
		}
	}
}

/// Any grant for good wins. Otherwise each rental runs for its days from
/// when it was bought, or from when the one before it ends if that is later.
pub(crate) fn held(
	grants: &[(DateTimeWithTimeZone, Option<i32>)],
	now: DateTimeWithTimeZone,
) -> Held {
	if grants.iter().any(|(_, days)| days.is_none()) {
		return Held::Permanent;
	}

	let mut rentals: Vec<(DateTimeWithTimeZone, i32)> = grants
		.iter()
		.filter_map(|(at, days)| Some((*at, (*days)?)))
		.collect();
	rentals.sort_unstable();

	let end = rentals.into_iter().fold(None, |end, (at, days)| {
		Some(end.map_or(at, |end: DateTimeWithTimeZone| end.max(at)) + Duration::days(days.into()))
	});
	match end {
		Some(end) if end > now => Held::Until(end),
		_ => Held::Gone,
	}
}

/// What `settle` did to one cosmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
	Added(Held),
	Changed(Held),
	Removed,
	Unchanged,
}

pub(crate) async fn record(
	db: &impl ConnectionTrait,
	player_id: i32,
	cosmetic_ids: &[i32],
	provider: TransactionProvider,
	transaction_id: Option<i32>,
	transaction_line_id: Option<i64>,
	days: Option<i32>,
) -> Result<(), DbErr> {
	if cosmetic_ids.is_empty() {
		return Ok(());
	}

	OwnershipGrant::insert_many(cosmetic_ids.iter().map(|&cosmetic_id| {
		ownership_grant::ActiveModel {
			id: ActiveValue::NotSet,
			player_id: Set(player_id),
			cosmetic_id: Set(cosmetic_id),
			provider: Set(provider.clone()),
			transaction_id: Set(transaction_id),
			transaction_line_id: Set(transaction_line_id),
			days: Set(days),
			granted_at: ActiveValue::NotSet,
			active: Set(true),
		}
	}))
	.exec_without_returning(db)
	.await?;

	Ok(())
}

/// Turns the grants of these lines on or off, returning the ones that moved.
pub(crate) async fn set_lines_active(
	db: &impl ConnectionTrait,
	line_ids: &[i64],
	active: bool,
) -> Result<Vec<ownership_grant::Model>, DbErr> {
	if line_ids.is_empty() {
		return Ok(Vec::new());
	}

	OwnershipGrant::update_many()
		.col_expr(ownership_grant::Column::Active, Expr::value(active))
		.filter(ownership_grant::Column::TransactionLineId.is_in(line_ids.to_vec()))
		.filter(ownership_grant::Column::Active.eq(!active))
		.exec_with_returning(db)
		.await
}

/// Brings `player_owned_cosmetic` in line with the player's active grants.
pub(crate) async fn settle(
	db: &impl ConnectionTrait,
	player_id: i32,
	cosmetic_ids: &[i32],
) -> Result<HashMap<i32, Settled>, DbErr> {
	let ids: Vec<i32> = cosmetic_ids
		.iter()
		.copied()
		.collect::<HashSet<_>>()
		.into_iter()
		.collect();
	if ids.is_empty() {
		return Ok(HashMap::new());
	}

	let mut grants: HashMap<i32, Vec<ownership_grant::Model>> = HashMap::new();
	for grant in OwnershipGrant::find()
		.filter(ownership_grant::Column::PlayerId.eq(player_id))
		.filter(ownership_grant::Column::CosmeticId.is_in(ids.clone()))
		.filter(ownership_grant::Column::Active.eq(true))
		.order_by_asc(ownership_grant::Column::GrantedAt)
		.order_by_asc(ownership_grant::Column::Id)
		.all(db)
		.await?
	{
		grants.entry(grant.cosmetic_id).or_default().push(grant);
	}

	let rows: HashMap<i32, player_owned_cosmetic::Model> = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(player_id))
		.filter(player_owned_cosmetic::Column::CosmeticId.is_in(ids.clone()))
		.all(db)
		.await?
		.into_iter()
		.map(|row| (row.cosmetic_id, row))
		.collect();

	let now = Utc::now().fixed_offset();
	let mut settled = HashMap::with_capacity(ids.len());
	for cosmetic_id in ids {
		let own = grants.remove(&cosmetic_id).unwrap_or_default();
		let held = held(
			&own.iter()
				.map(|grant| (grant.granted_at, grant.days))
				.collect::<Vec<_>>(),
			now,
		);
		let row = rows.get(&cosmetic_id);

		let outcome = match (row, own.last(), held) {
			(None, _, Held::Gone) => Settled::Unchanged,
			(Some(_), _, Held::Gone) => {
				PlayerOwnedCosmetic::delete_many()
					.filter(player_owned_cosmetic::Column::PlayerId.eq(player_id))
					.filter(player_owned_cosmetic::Column::CosmeticId.eq(cosmetic_id))
					.exec(db)
					.await?;
				Settled::Removed
			}
			(Some(row), _, _) if row.expires_at == held.expires_at() => Settled::Unchanged,
			// The newest grant is the one the row is attributed to.
			(row, Some(latest), _) => {
				let active = player_owned_cosmetic::ActiveModel {
					player_id: Set(player_id),
					cosmetic_id: Set(cosmetic_id),
					acquired_via: Set(latest.provider.clone()),
					transaction_id: Set(latest.transaction_id),
					transaction_line_id: Set(latest.transaction_line_id),
					acquired_at: ActiveValue::NotSet,
					expires_at: Set(held.expires_at()),
				};
				if row.is_some() {
					PlayerOwnedCosmetic::update(active).exec(db).await?;
					Settled::Changed(held)
				} else {
					PlayerOwnedCosmetic::insert(active)
						.exec_without_returning(db)
						.await?;
					Settled::Added(held)
				}
			}
			(_, None, _) => Settled::Unchanged,
		};
		settled.insert(cosmetic_id, outcome);
	}

	Ok(settled)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(day: i64) -> DateTimeWithTimeZone {
		"2026-01-01T00:00:00Z"
			.parse::<DateTimeWithTimeZone>()
			.unwrap()
			+ Duration::days(day)
	}

	#[test]
	fn rentals_run_back_to_back_and_a_refund_takes_back_its_own_days() {
		// Bought on day 0 for 30 days, extended on day 10 for 30 more.
		let both = [(at(0), Some(30)), (at(10), Some(30))];
		assert_eq!(held(&both, at(20)), Held::Until(at(60)));

		// Refunding the extension leaves the first 30 days.
		assert_eq!(held(&both[..1], at(20)), Held::Until(at(30)));
		// Refunding the first leaves the extension, counted from when it was
		// bought.
		assert_eq!(held(&both[1..], at(20)), Held::Until(at(40)));
	}

	#[test]
	fn a_lapsed_rental_restarts_from_the_new_purchase() {
		let grants = [(at(0), Some(7)), (at(30), Some(7))];
		assert_eq!(held(&grants, at(31)), Held::Until(at(37)));
	}

	#[test]
	fn owning_for_good_wins_and_nothing_left_is_gone() {
		assert_eq!(held(&[(at(0), Some(7)), (at(1), None)], at(2)), Held::Permanent);
		assert_eq!(held(&[(at(0), Some(7))], at(8)), Held::Gone);
		assert_eq!(held(&[], at(0)), Held::Gone);
	}
}
