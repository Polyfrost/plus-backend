use std::time::Duration;

use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::BodySlot;
use sea_orm::DatabaseConnection;
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use crate::api::state::realtime::PlaytimeSessions;

/// Capacity of the persistence channels before senders start to wait.
const PERSIST_QUEUE_SIZE: usize = 256;
/// How often accrued playtime is committed to the database.
const PLAYTIME_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// A pending change to a player's equipped cosmetic in a single body slot.
#[derive(Debug, Clone)]
pub struct EquipmentPersistence {
	pub player: Uuid,
	pub slot: BodySlot,
	pub cosmetic_id: Option<i32>,
}

/// A pending change to a player's particle color.
#[derive(Debug, Clone)]
pub struct ParticleColorPersistence {
	pub player: Uuid,
	pub color: Option<i32>,
}

/// Spawns the equipment writer and returns the channel feeding it.
pub(super) fn spawn_equipment_queue(
	database: DatabaseConnection,
) -> mpsc::Sender<EquipmentPersistence> {
	let (tx, rx) = mpsc::channel(PERSIST_QUEUE_SIZE);
	tokio::spawn(persist_equipment_queue(database, rx));
	tx
}

/// Spawns the particle color writer and returns the channel feeding it.
pub(super) fn spawn_particle_color_queue(
	database: DatabaseConnection,
) -> mpsc::Sender<ParticleColorPersistence> {
	let (tx, rx) = mpsc::channel(PERSIST_QUEUE_SIZE);
	tokio::spawn(persist_particle_color_queue(database, rx));
	tx
}

/// Spawns the periodic playtime flusher for the given session map.
pub(super) fn spawn_playtime_flush(
	database: DatabaseConnection,
	playtime: PlaytimeSessions,
) {
	tokio::spawn(flush_playtime_loop(database, playtime));
}

async fn persist_equipment_queue(
	database: DatabaseConnection,
	mut rx: mpsc::Receiver<EquipmentPersistence>,
) {
	use entities::{player_equipped_cosmetic, prelude::*, user};
	use sea_orm::{
		ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::OnConflict,
	};

	while let Some(update) = rx.recv().await {
		let result = async {
			let Some(player) = User::find()
				.filter(user::Column::MinecraftUuid.eq(update.player))
				.one(&database)
				.await?
			else {
				return Ok::<(), sea_orm::DbErr>(());
			};

			if let Some(cosmetic_id) = update.cosmetic_id {
				PlayerEquippedCosmetic::insert(player_equipped_cosmetic::ActiveModel {
					player_id: Set(player.id),
					slot: Set(update.slot),
					cosmetic_id: Set(cosmetic_id),
					updated_at: ActiveValue::NotSet,
				})
				.on_conflict(
					OnConflict::columns([
						player_equipped_cosmetic::Column::PlayerId,
						player_equipped_cosmetic::Column::Slot,
					])
					.update_column(player_equipped_cosmetic::Column::CosmeticId)
					.to_owned(),
				)
				.exec(&database)
				.await?;
			} else {
				PlayerEquippedCosmetic::delete_many()
					.filter(player_equipped_cosmetic::Column::PlayerId.eq(player.id))
					.filter(player_equipped_cosmetic::Column::Slot.eq(update.slot))
					.exec(&database)
					.await?;
			}

			Ok(())
		}
		.await;

		if let Err(error) = result {
			warn!("Unable to persist websocket equipment update: {error}");
		}
	}
}

async fn persist_particle_color_queue(
	database: DatabaseConnection,
	mut rx: mpsc::Receiver<ParticleColorPersistence>,
) {
	use entities::{prelude::*, user};
	use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

	while let Some(update) = rx.recv().await {
		let result = async {
			let Some(player) = User::find()
				.filter(user::Column::MinecraftUuid.eq(update.player))
				.one(&database)
				.await?
			else {
				return Ok::<(), sea_orm::DbErr>(());
			};

			let mut player: user::ActiveModel = player.into();
			player.particle_color = Set(update.color);
			player.update(&database).await?;

			Ok(())
		}
		.await;

		if let Err(error) = result {
			warn!("Unable to persist websocket particle color update: {error}");
		}
	}
}

async fn flush_playtime_loop(database: DatabaseConnection, playtime: PlaytimeSessions) {
	let mut interval = tokio::time::interval(PLAYTIME_FLUSH_INTERVAL);
	interval.tick().await;

	loop {
		interval.tick().await;
		let now = Utc::now();

		// Claim every session's outstanding window up front so the lock is
		// released before any database work happens.
		let pending: Vec<(i32, DateTime<Utc>)> = {
			let mut guard = playtime.write().await;
			guard
				.values_mut()
				.map(|session| {
					let from = session.last_accounted_at;
					session.last_accounted_at = now;
					(session.player_id, from)
				})
				.collect()
		};

		for (player_id, from) in pending {
			if let Err(error) =
				crate::database::accrue_playtime(&database, player_id, from, now, false)
					.await
			{
				warn!("Unable to flush playtime for player {player_id}: {error}");
			}
		}
	}
}
