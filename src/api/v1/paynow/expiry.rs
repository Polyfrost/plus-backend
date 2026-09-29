use std::{collections::HashMap, time::Duration};

use chrono::Utc;
use entities::{
	player_owned_cosmetic,
	prelude::*,
	sea_orm_active_enums::{OwnershipEventKind, TransactionProvider},
};
use sea_orm::{DbErr, TransactionTrait as _, prelude::*};
use tracing::warn;

use super::{
	grant::{Grants, cosmetics_by_id, uuids_by_id},
	webhook::broadcast,
};
use crate::api::ApiState;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

pub(in crate::api) fn spawn_expiry_sweeper(state: ApiState) {
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(SWEEP_INTERVAL);
		loop {
			interval.tick().await;

			match expire(&state).await {
				Ok(grants) => broadcast(&state, grants, true).await,
				Err(error) => warn!("Unable to expire rentals: {error}"),
			}
		}
	});
}

/// Removes every rental whose time is up.
async fn expire(state: &ApiState) -> Result<Grants, DbErr> {
	let txn = state.database.begin().await?;

	// One statement, so a rental extended mid-sweep is never counted as gone.
	let expired = PlayerOwnedCosmetic::delete_many()
		.filter(player_owned_cosmetic::Column::ExpiresAt.lte(Utc::now().fixed_offset()))
		.exec_with_returning(&txn)
		.await?;
	if expired.is_empty() {
		return Ok(Grants::new());
	}

	type Source = (
		i32,
		TransactionProvider,
		Option<i32>,
		Option<i64>,
		Option<DateTimeWithTimeZone>,
	);
	let mut by_source: HashMap<Source, Vec<i32>> = HashMap::new();
	for row in &expired {
		by_source
			.entry((
				row.player_id,
				row.acquired_via.clone(),
				row.transaction_id,
				row.transaction_line_id,
				row.expires_at,
			))
			.or_default()
			.push(row.cosmetic_id);
	}
	for ((player_id, provider, transaction_id, line_id, expires_at), cosmetic_ids) in
		&by_source
	{
		crate::database::record_ownership_events(
			&txn,
			*player_id,
			cosmetic_ids,
			OwnershipEventKind::Expired,
			provider.clone(),
			*transaction_id,
			*line_id,
			*expires_at,
		)
		.await?;
	}

	let cosmetics =
		cosmetics_by_id(&txn, expired.iter().map(|row| row.cosmetic_id).collect()).await?;
	let uuids = uuids_by_id(&txn, expired.iter().map(|row| row.player_id).collect()).await?;
	txn.commit().await?;

	let mut grants = Grants::new();
	for row in &expired {
		if let (Some(uuid), Some(cosmetic)) =
			(uuids.get(&row.player_id), cosmetics.get(&row.cosmetic_id))
		{
			grants.entry(*uuid).or_default().push(cosmetic);
		}
	}

	Ok(grants)
}
