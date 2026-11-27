use std::{
	collections::{BTreeMap, HashMap},
	hash::Hash,
	sync::{Arc, Mutex, MutexGuard, PoisonError},
	time::Duration,
};

use chrono::{DateTime, Utc};
use entities::{
	cosmetic, cosmetic_allowed_slot, prelude::*, sea_orm_active_enums::BodySlot,
};
use sea_orm::{
	ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter,
	QueryOrder, QuerySelect, Select,
};
use tokio::sync::{Notify, mpsc};
use tracing::warn;
use uuid::Uuid;

use crate::api::{
	state::realtime::PlaytimeSessions,
	v0::{
		bonuses::{BonusCatalog, record_completions},
		outfits::{mirror_active_outfit_particle_color, mirror_active_outfit_slots},
	},
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

pub trait PersistKey<K> {
	fn key(&self) -> K;
}

impl PersistKey<(Uuid, BodySlot)> for EquipmentPersistence {
	fn key(&self) -> (Uuid, BodySlot) {
		(self.player, self.slot.clone())
	}
}

impl PersistKey<Uuid> for ParticleColorPersistence {
	fn key(&self) -> Uuid {
		self.player
	}
}

#[derive(Debug, Clone)]
pub struct PersistQueue<T, K = Uuid> {
	tx: mpsc::Sender<(u64, T)>,
	order: Arc<WriteOrder<K>>,
}

impl<T: PersistKey<K>, K: Eq + Hash + Clone> PersistQueue<T, K> {
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
		let seq = self.order.begin(update.key());
		if self.tx.try_send((seq, update)).is_err() {
			self.order.finish(seq);
		}
	}

	pub fn supersede(&self, key: K) -> Option<Supersession<K>> {
		let mark = self.order.supersede(key)?;
		Some(Supersession {
			order: self.order.clone(),
			mark,
			confirmed: false,
		})
	}

	pub async fn flushed(&self) {
		self.order.flushed().await;
	}
}

#[derive(Debug)]
pub struct Supersession<K: Eq + Hash + Clone> {
	order: Arc<WriteOrder<K>>,
	mark: u64,
	confirmed: bool,
}

impl<K: Eq + Hash + Clone> Supersession<K> {
	pub fn confirm(mut self) {
		self.confirmed = true;
		self.order.resolve(self.mark, true);
	}
}

impl<K: Eq + Hash + Clone> Drop for Supersession<K> {
	fn drop(&mut self) {
		if !self.confirmed {
			self.order.resolve(self.mark, false);
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
	Write,
	Skip,
	Wait,
}

#[derive(Debug)]
struct WriteOrder<K> {
	state: Mutex<WriteOrderState<K>>,
	finished: Notify,
}

impl<K> Default for WriteOrder<K> {
	fn default() -> Self {
		Self {
			state: Mutex::new(WriteOrderState {
				next: 0,
				pending: BTreeMap::new(),
				superseded: HashMap::new(),
				tentative: BTreeMap::new(),
			}),
			finished: Notify::new(),
		}
	}
}

#[derive(Debug)]
struct WriteOrderState<K> {
	next: u64,
	pending: BTreeMap<u64, K>,
	superseded: HashMap<K, u64>,
	tentative: BTreeMap<u64, K>,
}

impl<K> WriteOrderState<K> {
	fn forget_settled_overwrites(&mut self) {
		match self.pending.first_key_value().map(|(&oldest, _)| oldest) {
			Some(oldest) => self.superseded.retain(|_, superseded| *superseded > oldest),
			None => self.superseded.clear(),
		}
	}
}

impl<K: Eq + Hash + Clone> WriteOrder<K> {
	fn state(&self) -> MutexGuard<'_, WriteOrderState<K>> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn begin(&self, key: K) -> u64 {
		let mut state = self.state();
		let seq = state.next;
		state.next += 1;
		state.pending.insert(seq, key);
		seq
	}

	fn supersede(&self, key: K) -> Option<u64> {
		let mut state = self.state();
		if !state.pending.values().any(|pending| *pending == key) {
			return None;
		}

		let mark = state.next;
		state.next += 1;
		state.tentative.insert(mark, key);
		Some(mark)
	}

	fn resolve(&self, mark: u64, confirmed: bool) {
		{
			let mut state = self.state();
			let Some(key) = state.tentative.remove(&mark) else {
				return;
			};
			if confirmed {
				let superseded = state.superseded.entry(key).or_insert(mark);
				*superseded = (*superseded).max(mark);
				state.forget_settled_overwrites();
			}
		}
		self.finished.notify_waiters();
	}

	fn verdict(&self, key: &K, seq: u64) -> Verdict {
		let state = self.state();
		if state
			.superseded
			.get(key)
			.is_some_and(|&superseded| superseded > seq)
		{
			return Verdict::Skip;
		}
		if state
			.tentative
			.iter()
			.any(|(&mark, tentative)| mark > seq && tentative == key)
		{
			return Verdict::Wait;
		}
		Verdict::Write
	}

	async fn settled(&self, key: &K, seq: u64) -> Verdict {
		loop {
			let resolved = self.finished.notified();
			match self.verdict(key, seq) {
				Verdict::Wait => resolved.await,
				verdict => return verdict,
			}
		}
	}

	fn finish(&self, seq: u64) {
		{
			let mut state = self.state();
			state.pending.remove(&seq);
			state.forget_settled_overwrites();
		}
		self.finished.notify_waiters();
	}

	fn settled_before(&self, seq: u64) -> bool {
		self.state()
			.pending
			.first_key_value()
			.is_none_or(|(&oldest, _)| oldest >= seq)
	}

	async fn flushed(&self) {
		let target = self.state().next;
		loop {
			let finished = self.finished.notified();
			if self.settled_before(target) {
				return;
			}
			finished.await;
		}
	}
}

pub(crate) fn holding_slot_rules(cosmetic_ids: impl IntoIterator<Item = i32>) -> Select<Cosmetic> {
	let mut cosmetic_ids = cosmetic_ids.into_iter().collect::<Vec<_>>();
	cosmetic_ids.sort_unstable();
	cosmetic_ids.dedup();
	Cosmetic::find()
		.filter(cosmetic::Column::Id.is_in(cosmetic_ids))
		.order_by_asc(cosmetic::Column::Id)
		.lock_shared()
}

async fn still_allowed<C: ConnectionTrait>(
	db: &C,
	cosmetic_id: i32,
	slot: &BodySlot,
) -> Result<bool, DbErr> {
	if holding_slot_rules([cosmetic_id]).all(db).await?.is_empty() {
		return Ok(false);
	}
	Ok(CosmeticAllowedSlot::find()
		.filter(cosmetic_allowed_slot::Column::CosmeticId.eq(cosmetic_id))
		.filter(cosmetic_allowed_slot::Column::Slot.eq(slot.clone()))
		.one(db)
		.await?
		.is_some())
}

fn settled_equip(requested: Option<i32>, allowed: bool) -> Option<i32> {
	requested.filter(|_| allowed)
}

async fn wearable_equip<C: ConnectionTrait>(
	db: &C,
	update: &EquipmentPersistence,
) -> Result<Option<i32>, DbErr> {
	let Some(cosmetic_id) = update.cosmetic_id else {
		return Ok(None);
	};
	let allowed = still_allowed(db, cosmetic_id, &update.slot).await?;
	Ok(settled_equip(Some(cosmetic_id), allowed))
}

/// Spawns the equipment writer and returns the channel feeding it.
pub(super) fn spawn_equipment_queue(
	database: DatabaseConnection,
	bonuses: BonusCatalog,
) -> PersistQueue<EquipmentPersistence, (Uuid, BodySlot)> {
	PersistQueue::spawn(|rx, order| persist_equipment_queue(database, bonuses, rx, order))
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
	bonuses: BonusCatalog,
	mut rx: mpsc::Receiver<(u64, EquipmentPersistence)>,
	order: Arc<WriteOrder<(Uuid, BodySlot)>>,
) {
	use entities::{player_equipped_cosmetic, user};
	use sea_orm::{
		ActiveValue, Set, TransactionTrait,
		sea_query::{Expr, OnConflict},
	};

	while let Some((seq, update)) = rx.recv().await {
		let key = update.key();
		let result = loop {
			if order.settled(&key, seq).await == Verdict::Skip {
				break Ok(None);
			}
			let attempt = async {
				let txn = database.begin().await?;
				let Some(player) = User::find()
					.filter(user::Column::MinecraftUuid.eq(update.player))
					.lock_exclusive()
					.one(&txn)
					.await?
				else {
					return Ok::<_, sea_orm::DbErr>(Some(None));
				};
				match order.verdict(&key, seq) {
					Verdict::Skip => return Ok(Some(None)),
					Verdict::Wait => {
						txn.rollback().await?;
						return Ok(None);
					}
					Verdict::Write => {}
				}

				let cosmetic_id = wearable_equip(&txn, &update).await?;
				if let Some(cosmetic_id) = cosmetic_id {
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
					[(update.slot.clone(), cosmetic_id)],
				)
				.await?;
				let recorded = cosmetic_id.is_some()
					&& record_completions(&txn, &bonuses, player.id).await?;

				txn.commit().await?;
				Ok(Some(recorded.then_some(player.id)))
			}
			.await;
			match attempt {
				Ok(None) => continue,
				Ok(Some(recorded)) => break Ok(recorded),
				Err(error) => break Err(error),
			}
		};
		order.finish(seq);

		match result {
			Ok(Some(player_id)) => bonuses.recorded(update.player, player_id),
			Ok(None) => {}
			Err(error) => warn!("Unable to persist websocket equipment update: {error}"),
		}
	}
}

async fn persist_particle_color_queue(
	database: DatabaseConnection,
	mut rx: mpsc::Receiver<(u64, ParticleColorPersistence)>,
	order: Arc<WriteOrder<Uuid>>,
) {
	use entities::user;
	use sea_orm::{TransactionTrait, sea_query::Expr};

	while let Some((seq, update)) = rx.recv().await {
		let key = update.key();
		let result = loop {
			if order.settled(&key, seq).await == Verdict::Skip {
				break Ok(());
			}
			let attempt = async {
				let txn = database.begin().await?;
				let Some(player) = User::find()
					.filter(user::Column::MinecraftUuid.eq(update.player))
					.lock_exclusive()
					.one(&txn)
					.await?
				else {
					return Ok::<_, sea_orm::DbErr>(true);
				};
				match order.verdict(&key, seq) {
					Verdict::Skip => return Ok(true),
					Verdict::Wait => {
						txn.rollback().await?;
						return Ok(false);
					}
					Verdict::Write => {}
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

				txn.commit().await?;
				Ok(true)
			}
			.await;
			match attempt {
				Ok(false) => continue,
				attempt => break attempt.map(drop),
			}
		};
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
	use std::{sync::Arc, time::Duration};

	use sea_orm::{DbBackend, QueryTrait};
	use tokio::{sync::mpsc, task::yield_now, time::timeout};
	use uuid::Uuid;

	use super::{
		ParticleColorPersistence, PersistQueue, Supersession, Verdict, WriteOrder,
		holding_slot_rules, settled_equip,
	};

	fn supersession(order: &Arc<WriteOrder<Uuid>>, player: Uuid) -> Option<Supersession<Uuid>> {
		let mark = order.supersede(player)?;
		Some(Supersession { order: order.clone(), mark, confirmed: false })
	}

	async fn settle() {
		for _ in 0..16 {
			yield_now().await;
		}
	}

	#[test]
	fn a_queued_equip_holds_the_slot_rules_and_clears_a_slot_it_lost() {
		assert_eq!(settled_equip(Some(5), true), Some(5));
		for (equip, allowed) in [(Some(5), false), (None, true), (None, false)] {
			assert_eq!(settled_equip(equip, allowed), None);
		}
		let sql = holding_slot_rules([9, 4, 9]).build(DbBackend::Postgres).to_string();
		assert!(sql.contains(r#""cosmetic"."id" IN (4, 9)"#), "{sql}");
		assert!(sql.ends_with(r#"ORDER BY "cosmetic"."id" ASC FOR SHARE"#), "{sql}");
	}

	#[test]
	fn a_confirmed_overwrite_skips_only_earlier_writes_of_its_key() {
		let order = Arc::new(WriteOrder::default());
		let (player, other) = (Uuid::new_v4(), Uuid::new_v4());
		assert!(supersession(&order, player).is_none());
		let earlier = order.begin(player);
		let overwrite = supersession(&order, player).expect("a write is pending");
		let later = order.begin(player);
		assert_eq!(order.verdict(&player, earlier), Verdict::Wait);
		assert_eq!(order.verdict(&player, later), Verdict::Write);
		overwrite.confirm();
		assert_eq!(order.verdict(&player, earlier), Verdict::Skip);
		assert_eq!(order.verdict(&player, later), Verdict::Write);
		assert_eq!(order.verdict(&other, earlier), Verdict::Write);

		let pending = order.begin(other);
		drop(supersession(&order, other).expect("the other key has a pending write"));
		assert_eq!(order.verdict(&other, pending), Verdict::Write);
	}

	#[tokio::test]
	async fn queued_writes_and_flushes_wait_only_for_what_came_before() {
		let order = Arc::new(WriteOrder::default());
		let player = Uuid::new_v4();
		let earlier = order.begin(player);
		let overwrite = supersession(&order, player).expect("a write is pending");
		let settled = tokio::spawn({
			let order = order.clone();
			async move { order.settled(&player, earlier).await }
		});
		let flush = tokio::spawn({
			let order = order.clone();
			async move { order.flushed().await }
		});
		settle().await;
		order.begin(player);
		assert!(!settled.is_finished() && !flush.is_finished());

		drop(overwrite);
		assert_eq!(timeout(Duration::from_secs(1), settled).await.unwrap().unwrap(), Verdict::Write);
		assert!(!flush.is_finished());
		order.finish(earlier);
		timeout(Duration::from_secs(1), flush).await.unwrap().unwrap();

		let (tx, _rx) = mpsc::channel(1);
		let queue: PersistQueue<ParticleColorPersistence> = PersistQueue { tx, order: Arc::new(WriteOrder::default()) };
		assert!(queue.supersede(player).is_none());
		queue.enqueue(ParticleColorPersistence { player, color: Some(1) });
		assert!(queue.supersede(player).is_some());
	}
}
