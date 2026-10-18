use std::{
	collections::{BTreeSet, HashMap},
	hash::Hash,
	sync::{Arc, Mutex, MutexGuard, PoisonError},
	time::Duration,
};

use chrono::{DateTime, Utc};
use entities::sea_orm_active_enums::BodySlot;
use sea_orm::DatabaseConnection;
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use crate::api::{
	state::realtime::PlaytimeSessions,
	v0::outfits::{mirror_active_outfit_particle_color, mirror_active_outfit_slots},
};

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

#[derive(Debug, Clone)]
pub struct PersistQueue<T, K = Uuid> {
	tx: mpsc::Sender<(u64, T)>,
	order: Arc<WriteOrder<K>>,
}

impl<T, K: Eq + Hash> PersistQueue<T, K> {
	fn spawn<F>(
		worker: impl FnOnce(mpsc::Receiver<(u64, T)>, Arc<WriteOrder<K>>) -> F,
	) -> Self
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let (tx, rx) = mpsc::channel(PERSIST_QUEUE_SIZE);
		let order = Arc::new(WriteOrder::default());
		tokio::spawn(worker(rx, order.clone()));
		Self { tx, order }
	}

	pub fn enqueue(&self, update: T) {
		let seq = self.order.begin();
		if self.tx.try_send((seq, update)).is_err() {
			self.order.finish(seq);
		}
	}

	pub fn supersede(&self, key: K) {
		self.order.supersede(key);
	}
}

#[derive(Debug)]
struct WriteOrder<K>(Mutex<WriteOrderState<K>>);

impl<K> Default for WriteOrder<K> {
	fn default() -> Self {
		Self(Mutex::new(WriteOrderState {
			next: 0,
			pending: BTreeSet::new(),
			superseded: HashMap::new(),
		}))
	}
}

#[derive(Debug)]
struct WriteOrderState<K> {
	next: u64,
	pending: BTreeSet<u64>,
	superseded: HashMap<K, u64>,
}

impl<K: Eq + Hash> WriteOrder<K> {
	fn state(&self) -> MutexGuard<'_, WriteOrderState<K>> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn begin(&self) -> u64 {
		let mut state = self.state();
		let seq = state.next;
		state.next += 1;
		state.pending.insert(seq);
		seq
	}

	fn supersede(&self, key: K) {
		let mut state = self.state();
		if state.pending.is_empty() {
			return;
		}

		let seq = state.next;
		state.next += 1;
		state.superseded.insert(key, seq);
	}

	fn is_superseded(&self, key: K, seq: u64) -> bool {
		self.state()
			.superseded
			.get(&key)
			.is_some_and(|&superseded| superseded > seq)
	}

	fn finish(&self, seq: u64) {
		let mut state = self.state();
		state.pending.remove(&seq);
		match state.pending.first().copied() {
			Some(oldest) => state.superseded.retain(|_, superseded| *superseded > oldest),
			None => state.superseded.clear(),
		}
	}
}

/// Spawns the equipment writer and returns the channel feeding it.
pub(super) fn spawn_equipment_queue(
	database: DatabaseConnection,
) -> PersistQueue<EquipmentPersistence, (Uuid, BodySlot)> {
	PersistQueue::spawn(|rx, order| persist_equipment_queue(database, rx, order))
}

/// Spawns the particle color writer and returns the channel feeding it.
pub(super) fn spawn_particle_color_queue(
	database: DatabaseConnection,
) -> PersistQueue<ParticleColorPersistence> {
	PersistQueue::spawn(|rx, order| persist_particle_color_queue(database, rx, order))
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
	mut rx: mpsc::Receiver<(u64, EquipmentPersistence)>,
	order: Arc<WriteOrder<(Uuid, BodySlot)>>,
) {
	use entities::{player_equipped_cosmetic, prelude::*, user};
	use sea_orm::{
		ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QuerySelect, Set,
		TransactionTrait,
		sea_query::{Expr, OnConflict},
	};

	while let Some((seq, update)) = rx.recv().await {
		let result = async {
			let txn = database.begin().await?;
			let Some(player) = User::find()
				.filter(user::Column::MinecraftUuid.eq(update.player))
				.lock_exclusive()
				.one(&txn)
				.await?
			else {
				return Ok::<_, sea_orm::DbErr>(());
			};
			if order.is_superseded((update.player, update.slot.clone()), seq) {
				return Ok(());
			}

			if let Some(cosmetic_id) = update.cosmetic_id {
				PlayerEquippedCosmetic::insert(player_equipped_cosmetic::ActiveModel {
					player_id: Set(player.id),
					slot: Set(update.slot.clone()),
					cosmetic_id: Set(cosmetic_id),
					updated_at: ActiveValue::NotSet,
				})
				.on_conflict(
					OnConflict::columns([
						player_equipped_cosmetic::Column::PlayerId,
						player_equipped_cosmetic::Column::Slot,
					])
					.update_column(player_equipped_cosmetic::Column::CosmeticId)
					.value(
						player_equipped_cosmetic::Column::UpdatedAt,
						Expr::current_timestamp(),
					)
					.to_owned(),
				)
				.exec(&txn)
				.await?;
			} else {
				PlayerEquippedCosmetic::delete_many()
					.filter(player_equipped_cosmetic::Column::PlayerId.eq(player.id))
					.filter(
						player_equipped_cosmetic::Column::Slot.eq(update.slot.clone()),
					)
					.exec(&txn)
					.await?;
			}

			mirror_active_outfit_slots(
				&txn,
				player.id,
				player.active_outfit_id,
				[(update.slot, update.cosmetic_id)],
			)
			.await?;

			txn.commit().await
		}
		.await;
		order.finish(seq);

		if let Err(error) = result {
			warn!("Unable to persist websocket equipment update: {error}");
		}
	}
}

async fn persist_particle_color_queue(
	database: DatabaseConnection,
	mut rx: mpsc::Receiver<(u64, ParticleColorPersistence)>,
	order: Arc<WriteOrder<Uuid>>,
) {
	use entities::{prelude::*, user};
	use sea_orm::{
		ColumnTrait, EntityTrait, QueryFilter, QuerySelect, TransactionTrait,
		sea_query::Expr,
	};

	while let Some((seq, update)) = rx.recv().await {
		let result = async {
			let txn = database.begin().await?;
			let Some(player) = User::find()
				.filter(user::Column::MinecraftUuid.eq(update.player))
				.lock_exclusive()
				.one(&txn)
				.await?
			else {
				return Ok::<(), sea_orm::DbErr>(());
			};
			if order.is_superseded(update.player, seq) {
				return Ok(());
			}

			User::update_many()
				.col_expr(user::Column::ParticleColor, Expr::value(update.color))
				.filter(user::Column::Id.eq(player.id))
				.exec(&txn)
				.await?;
			mirror_active_outfit_particle_color(
				&txn,
				player.id,
				player.active_outfit_id,
				update.color,
			)
			.await?;

			txn.commit().await
		}
		.await;
		order.finish(seq);

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
		let (windows, session_ids): (Vec<_>, Vec<_>) = {
			let mut guard = playtime.write().await;
			guard
				.values_mut()
				.map(|session| {
					let from = session.last_accounted_at;
					session.last_accounted_at = now;
					((session.player_id, from, now), session.session_row_id)
				})
				.unzip()
		};

		if windows.is_empty() {
			continue;
		}

		if let Err(error) =
			crate::database::flush_playtime_windows(&database, &windows).await
		{
			warn!(
				"Unable to flush playtime for {} sessions: {error}",
				windows.len()
			);
		}

		if let Err(error) = heartbeat_sessions(&database, &session_ids, now).await {
			warn!("Unable to heartbeat play sessions: {error}");
		}
	}
}

/// Timestamps open sessions so the startup reaper can close them at their
/// last heartbeat.
async fn heartbeat_sessions(
	database: &DatabaseConnection,
	session_ids: &[i64],
	now: DateTime<Utc>,
) -> Result<(), sea_orm::DbErr> {
	use entities::{play_session, prelude::*};
	use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

	PlaySession::update_many()
		.col_expr(
			play_session::Column::LastHeartbeatAt,
			Expr::value(sea_orm::prelude::DateTimeWithTimeZone::from(now)),
		)
		.filter(play_session::Column::Id.is_in(session_ids.iter().copied()))
		.filter(play_session::Column::EndedAt.is_null())
		.exec(database)
		.await?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use uuid::Uuid;

	use super::WriteOrder;

	#[test]
	fn supersedes_only_earlier_writes() {
		let order = WriteOrder::default();
		let player = Uuid::new_v4();
		let earlier = order.begin();
		order.supersede(player);
		let later = order.begin();

		assert!(order.is_superseded(player, earlier));
		assert!(!order.is_superseded(player, later));
		assert!(!order.is_superseded(Uuid::new_v4(), earlier));
	}

	#[test]
	fn ignores_overwrites_with_nothing_pending() {
		let order = WriteOrder::default();
		let player = Uuid::new_v4();
		order.supersede(player);

		assert!(order.state().superseded.is_empty());
		assert!(!order.is_superseded(player, order.begin()));
	}

	#[test]
	fn forgets_overwrites_once_earlier_writes_finish() {
		let order = WriteOrder::default();
		let player = Uuid::new_v4();
		let earlier = order.begin();
		order.supersede(player);
		let later = order.begin();

		order.finish(earlier);
		assert!(order.state().superseded.is_empty());
		order.finish(later);
		assert!(order.state().pending.is_empty());
	}
}
