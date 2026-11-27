mod discovered;
mod preview;
mod resolve;

use std::{
	collections::{HashMap, HashSet},
	fmt::Display,
	future::{Future, ready},
	mem,
	sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError},
	time::Duration,
};

use aide::axum::ApiRouter;
use chrono::{DateTime, Utc};
use entities::{
	asset, cosmetic, cosmetic_set, cosmetic_set_requirement, player_equipped_cosmetic,
	player_set_discovery, prelude::*, user,
};
use futures::future::BoxFuture;
use moka::future::Cache;
use schemars::JsonSchema;
use sea_orm::{
	ActiveValue::{NotSet, Set},
	ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder,
	QuerySelect, Select, TransactionTrait, TryInsert, TryInsertResult, UpdateMany,
	sea_query::Expr,
};
use serde::Serialize;
use tokio::{
	sync::{Notify, OwnedMutexGuard, mpsc, oneshot},
	time::{Instant, sleep_until, timeout},
};
use tracing::warn;
use uuid::Uuid;

pub(crate) use self::preview::{admin_preview, admin_preview_doc};
use self::resolve::{EquippedItem, Requirement, SetRule, completed_sets, resolve_bonuses};
use crate::api::{
	ApiState,
	state::{PlayerRuntimeState, RealtimeConnection, RealtimeState},
	v0::{
		cosmetics::CachedAssetInfo,
		websocket::{
			broadcast_to_watchers, send_to_realtime_owner,
			structs::{ClientBoundPacket, WriteReceipt},
		},
	},
};

const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);
const WINDOW_RECHECK: Duration = CATALOG_TTL;
const WINDOW_RETRY: Duration = Duration::from_secs(30);
const RECEIPT_PATIENCE: Duration = Duration::from_secs(30);

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/bonuses",
		ApiRouter::new()
			.merge(preview::router())
			.merge(discovered::router()),
	)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct BonusRef {
	pub id: i32,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub url: Option<String>,
	pub hash: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SetCatalog {
	sets: Vec<SetRule>,
	names: HashMap<i32, String>,
	refs: HashMap<i32, BonusRef>,
	variants: HashMap<i32, (i32, Option<String>)>,
}

impl SetCatalog {
	fn item(&self, cosmetic_id: i32) -> EquippedItem {
		let (group_id, variant_key) = match self.variants.get(&cosmetic_id) {
			Some((group_id, variant_key)) => (Some(*group_id), variant_key.clone()),
			None => (None, None),
		};
		EquippedItem {
			cosmetic_id,
			group_id,
			variant_key,
		}
	}

	pub(crate) fn items(&self, ids: impl IntoIterator<Item = i32>) -> Vec<EquippedItem> {
		let mut ids: Vec<i32> = ids.into_iter().collect();
		ids.sort_unstable();
		ids.dedup();
		ids.into_iter().map(|id| self.item(id)).collect()
	}

	pub(crate) fn resolve_items(
		&self,
		ids: impl IntoIterator<Item = i32>,
		now: DateTime<Utc>,
	) -> Vec<i32> {
		if self.sets.is_empty() {
			return Vec::new();
		}
		resolve_bonuses(&self.items(ids), &self.sets, now)
	}

	pub(crate) fn active(
		&self,
		ids: impl IntoIterator<Item = i32>,
		now: DateTime<Utc>,
	) -> Vec<BonusRef> {
		self.refs(&self.resolve_items(ids, now))
	}

	fn completed(&self, ids: impl IntoIterator<Item = i32>, now: DateTime<Utc>) -> Vec<i32> {
		let mut completed = completed_sets(&self.items(ids), &self.sets, now);
		completed.sort_unstable();
		completed
	}

	fn completions(
		&self,
		ids: impl IntoIterator<Item = i32>,
		now: DateTime<Utc>,
	) -> Vec<(i32, Vec<i32>)> {
		let items = self.items(ids);
		completed_sets(&items, &self.sets, now)
			.into_iter()
			.filter_map(|id| Some((id, self.matched_items(id, &items)?)))
			.collect()
	}

	pub(crate) fn refs(&self, ids: &[i32]) -> Vec<BonusRef> {
		ids.iter()
			.filter_map(|id| self.refs.get(id).cloned())
			.collect()
	}

	fn boundaries(&self) -> impl Iterator<Item = DateTime<Utc>> + '_ {
		self.sets
			.iter()
			.flat_map(|set| [set.active_from, set.active_until])
			.flatten()
	}

	fn next_boundary(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
		self.boundaries().filter(|at| *at > after).min()
	}

	fn crossed_boundary(&self, after: DateTime<Utc>, until: DateTime<Utc>) -> bool {
		self.boundaries().any(|at| at > after && at <= until)
	}

	fn matched_items(&self, set_id: i32, items: &[EquippedItem]) -> Option<Vec<i32>> {
		self.sets
			.iter()
			.find(|set| set.id == set_id)?
			.matched_items(items)
	}
}

type Recorded = (Uuid, i32);

#[derive(Debug, Clone)]
pub(crate) struct BonusCatalog {
	cache: Cache<(), Arc<SetCatalog>>,
	changed: Arc<Notify>,
	recorded: mpsc::UnboundedSender<Recorded>,
	recorded_rx: Arc<StdMutex<Option<mpsc::UnboundedReceiver<Recorded>>>>,
}

impl BonusCatalog {
	pub(crate) fn recorded(&self, player: Uuid, player_id: i32) {
		let _ = self.recorded.send((player, player_id));
	}
}

pub(crate) fn new_catalog_cache() -> BonusCatalog {
	let (recorded, recorded_rx) = mpsc::unbounded_channel();
	BonusCatalog {
		cache: Cache::builder().time_to_live(CATALOG_TTL).build(),
		changed: Arc::new(Notify::new()),
		recorded,
		recorded_rx: Arc::new(StdMutex::new(Some(recorded_rx))),
	}
}

#[derive(Debug, Clone, Default)]
pub struct BonusRetries {
	players: Arc<StdMutex<HashSet<Uuid>>>,
	marked: Arc<Notify>,
}

impl BonusRetries {
	fn players(&self) -> MutexGuard<'_, HashSet<Uuid>> {
		self.players.lock().unwrap_or_else(PoisonError::into_inner)
	}

	pub fn mark(&self, player: Uuid) {
		self.players().insert(player);
		self.marked.notify_one();
	}

	fn take(&self) -> Vec<Uuid> {
		mem::take(&mut *self.players()).into_iter().collect()
	}

	fn is_empty(&self) -> bool {
		self.players().is_empty()
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CatalogError {
	#[error("Unable to query database: {0}")]
	Database(#[from] DbErr),
	#[error("Unable to read assets from object storage: {0}")]
	S3(#[from] s3::error::S3Error),
}

pub(crate) fn requirement_from_row(
	row: &cosmetic_set_requirement::Model,
) -> Option<Requirement> {
	match (row.cosmetic_id, row.group_id) {
		(Some(cosmetic_id), None) if row.variant_key.is_none() => {
			Some(Requirement::Cosmetic(cosmetic_id))
		}
		(None, Some(group_id)) => Some(Requirement::Group {
			group_id,
			variant_key: row.variant_key.clone(),
		}),
		_ => None,
	}
}

async fn load_rules<C: ConnectionTrait>(
	db: &C,
) -> Result<(SetCatalog, Vec<cosmetic_set::Model>), DbErr> {
	let rows = CosmeticSet::find()
		.find_with_related(CosmeticSetRequirement)
		.all(db)
		.await?;
	build_rules(db, rows).await
}

fn locked_sets(set_ids: Vec<i32>) -> Select<CosmeticSet> {
	CosmeticSet::find()
		.filter(cosmetic_set::Column::Id.is_in(set_ids))
		.order_by_asc(cosmetic_set::Column::Id)
		.lock_shared()
}

async fn locked_rules<C: ConnectionTrait>(
	db: &C,
	set_ids: Vec<i32>,
) -> Result<SetCatalog, DbErr> {
	let sets = locked_sets(set_ids).all(db).await?;
	if sets.is_empty() {
		return Ok(SetCatalog::default());
	}
	let mut requirements: HashMap<i32, Vec<cosmetic_set_requirement::Model>> = HashMap::new();
	for row in CosmeticSetRequirement::find()
		.filter(cosmetic_set_requirement::Column::SetId.is_in(sets.iter().map(|set| set.id)))
		.order_by_asc(cosmetic_set_requirement::Column::Id)
		.all(db)
		.await?
	{
		requirements.entry(row.set_id).or_default().push(row);
	}
	let rows = sets
		.into_iter()
		.map(|set| {
			let rows = requirements.remove(&set.id).unwrap_or_default();
			(set, rows)
		})
		.collect();
	Ok(build_rules(db, rows).await?.0)
}

async fn build_rules<C: ConnectionTrait>(
	db: &C,
	rows: Vec<(cosmetic_set::Model, Vec<cosmetic_set_requirement::Model>)>,
) -> Result<(SetCatalog, Vec<cosmetic_set::Model>), DbErr> {
	let group_ids: HashSet<i32> = rows
		.iter()
		.flat_map(|(_, requirements)| requirements.iter().filter_map(|row| row.group_id))
		.collect();
	let variants = if group_ids.is_empty() {
		HashMap::new()
	} else {
		Cosmetic::find()
			.filter(cosmetic::Column::GroupId.is_in(group_ids))
			.all(db)
			.await?
			.into_iter()
			.filter_map(|cosmetic| {
				Some((
					cosmetic.id,
					(cosmetic.group_id?, cosmetic.variant_key.clone()),
				))
			})
			.collect()
	};

	let mut catalog = SetCatalog {
		variants,
		..Default::default()
	};
	let mut kept = Vec::new();
	for (set, requirements) in rows {
		let Some(requirements) = requirements
			.iter()
			.map(requirement_from_row)
			.collect::<Option<Vec<_>>>()
		else {
			warn!(
				set = set.id,
				"Skipping a cosmetic set with a malformed requirement"
			);
			continue;
		};
		catalog.sets.push(set_rule(&set, requirements));
		catalog.names.insert(set.id, set.name.clone());
		kept.push(set);
	}

	Ok((catalog, kept))
}

async fn load_catalog(state: &ApiState) -> Result<Arc<SetCatalog>, CatalogError> {
	let (mut catalog, sets) = load_rules(&state.database).await?;

	let asset_ids: Vec<i32> = sets.iter().filter_map(|set| set.effect_asset_id).collect();
	let assets: HashMap<i32, asset::Model> = if asset_ids.is_empty() {
		HashMap::new()
	} else {
		Asset::find()
			.filter(asset::Column::Id.is_in(asset_ids))
			.all(&state.database)
			.await?
			.into_iter()
			.map(|asset| (asset.id, asset))
			.collect()
	};

	for set in sets {
		let asset = set.effect_asset_id.and_then(|id| assets.get(&id));
		let info = CachedAssetInfo::from_optional_asset(
			asset,
			state.asset_cache.clone(),
			state.s3_bucket.clone(),
		)
		.await?;
		catalog.refs.insert(
			set.id,
			BonusRef {
				id: set.id,
				url: CachedAssetInfo::asset_url(asset, &state.s3_public_url),
				hash: info.hash,
			},
		);
	}

	Ok(Arc::new(catalog))
}

pub(crate) async fn catalog(state: &ApiState) -> Option<Arc<SetCatalog>> {
	match state
		.bonus_catalog
		.cache
		.try_get_with((), load_catalog(state))
		.await
	{
		Ok(catalog) => Some(catalog),
		Err(error) => {
			warn!("Unable to load cosmetic sets: {error}");
			None
		}
	}
}

trait CompletionStore: Sync {
	fn rules(&self) -> impl Future<Output = Result<SetCatalog, DbErr>> + Send;
	fn equipped(&self, player_id: i32) -> impl Future<Output = Result<Vec<i32>, DbErr>> + Send;
	fn current(&self, set_ids: Vec<i32>) -> impl Future<Output = Result<SetCatalog, DbErr>> + Send;
	fn record(
		&self,
		player_id: i32,
		found: Vec<(i32, Vec<i32>)>,
	) -> impl Future<Output = Result<bool, DbErr>> + Send;
}

struct Committed<'a, C>(&'a C);

impl<C: ConnectionTrait + Sync> CompletionStore for Committed<'_, C> {
	async fn rules(&self) -> Result<SetCatalog, DbErr> {
		Ok(load_rules(self.0).await?.0)
	}

	async fn equipped(&self, player_id: i32) -> Result<Vec<i32>, DbErr> {
		Ok(equipped_cosmetics(player_id)
			.all(self.0)
			.await?
			.into_iter()
			.map(|row| row.cosmetic_id)
			.collect())
	}

	async fn current(&self, set_ids: Vec<i32>) -> Result<SetCatalog, DbErr> {
		locked_rules(self.0, set_ids).await
	}

	async fn record(&self, player_id: i32, found: Vec<(i32, Vec<i32>)>) -> Result<bool, DbErr> {
		if found.is_empty() {
			return Ok(false);
		}
		let inserted = discovery_insert(player_id, &found)
			.exec_without_returning(self.0)
			.await?;
		Ok(matches!(inserted, TryInsertResult::Inserted(rows) if rows > 0))
	}
}

async fn record_completed(
	store: &impl CompletionStore,
	cached: Option<Arc<SetCatalog>>,
	player_id: i32,
	now: DateTime<Utc>,
) -> Result<bool, DbErr> {
	let rules = match cached {
		Some(rules) => rules,
		None => Arc::new(store.rules().await?),
	};
	if rules.sets.is_empty() {
		return Ok(false);
	}
	let equipped = store.equipped(player_id).await?;
	let candidates = rules.completed(equipped.iter().copied(), now);
	if candidates.is_empty() {
		return Ok(false);
	}
	let found = store.current(candidates).await?.completions(equipped, now);
	if found.is_empty() {
		return Ok(false);
	}
	store.record(player_id, found).await
}

pub(crate) async fn record_completions<C: ConnectionTrait + Sync>(
	db: &C,
	catalog: &BonusCatalog,
	player_id: i32,
) -> Result<bool, DbErr> {
	let cached = catalog.cache.get(&()).await;
	record_completed(&Committed(db), cached, player_id, Utc::now()).await
}

async fn record_committed(
	state: &ApiState,
	catalog: Arc<SetCatalog>,
	player: Uuid,
) -> Result<Option<(i32, bool)>, DbErr> {
	let txn = state.database.begin().await?;
	let Some(user) = User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.lock_exclusive()
		.one(&txn)
		.await?
	else {
		return Ok(None);
	};
	let recorded = record_completed(&Committed(&txn), Some(catalog), user.id, Utc::now()).await?;
	txn.commit().await?;
	Ok(Some((user.id, recorded)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Discovery {
	Gained,
	Everything,
}

#[derive(Debug, PartialEq, Eq)]
struct BonusChange {
	player: Uuid,
	bonuses: Vec<BonusRef>,
	changed: bool,
}

fn plan_refresh(
	catalog: &SetCatalog,
	player: Uuid,
	runtime: &mut PlayerRuntimeState,
	discovery: Discovery,
	now: DateTime<Utc>,
) -> Option<BonusChange> {
	let bonuses = catalog.active(runtime.equipped.values().copied(), now);
	let changed = bonuses != runtime.bonuses;
	runtime.bonuses = bonuses.clone();
	let resend = discovery == Discovery::Everything && !bonuses.is_empty();
	(changed || resend).then_some(BonusChange {
		player,
		bonuses,
		changed,
	})
}

async fn refresh_player(
	realtime: &RealtimeState,
	player: Uuid,
	discovery: Discovery,
	catalog: impl Future<Output = Option<Arc<SetCatalog>>>,
	before_announcing: impl Future<Output = ()>,
) -> Option<Arc<SetCatalog>> {
	let _update = realtime.bonus_updates.lock(player).await;
	let Some(catalog) = catalog.await else {
		realtime.bonus_retries.mark(player);
		return None;
	};
	let change = {
		let mut player_runtime = realtime.player_runtime.write().await;
		player_runtime.get_mut(&player).and_then(|runtime| {
			plan_refresh(&catalog, player, runtime, discovery, Utc::now())
		})
	};
	let Some(change) = change else {
		return Some(catalog);
	};

	before_announcing.await;
	let bonuses = change.bonuses;
	let packet = || ClientBoundPacket::PlayerBonusesChanged {
		player,
		bonuses: bonuses.clone(),
	};
	if change.changed {
		broadcast_to_watchers(realtime, player, packet).await;
	}
	if change.changed || discovery == Discovery::Everything {
		send_to_realtime_owner(realtime, player, packet).await;
	}

	Some(catalog)
}

pub(crate) async fn refresh_bonuses(state: &ApiState, players: impl IntoIterator<Item = Uuid>) {
	let mut players = players.into_iter();
	while let Some(player) = players.next() {
		let refreshed = refresh_player(
			&state.realtime,
			player,
			Discovery::Gained,
			catalog(state),
			ready(()),
		)
		.await;
		if refreshed.is_none() {
			players.for_each(|player| state.realtime.bonus_retries.mark(player));
			return;
		}
	}
}

pub(crate) async fn rescan_bonuses(
	state: &ApiState,
	players: impl IntoIterator<Item = Uuid>,
	discovery: Discovery,
) {
	let mut players = players.into_iter();
	while let Some(player) = players.next() {
		let Some(catalog) = refresh_player(
			&state.realtime,
			player,
			discovery,
			catalog(state),
			ready(()),
		)
		.await
		else {
			players.for_each(|player| state.realtime.bonus_retries.mark(player));
			return;
		};
		let recorded = match record_committed(state, catalog.clone(), player).await {
			Ok(recorded) => recorded,
			Err(error) => {
				warn!("Unable to record set discoveries: {error}");
				state.realtime.bonus_retries.mark(player);
				continue;
			}
		};
		if let Some((player_id, recorded)) = recorded
			&& (recorded || discovery == Discovery::Everything)
		{
			deliver_with(state, catalog, player, player_id).await;
		}
	}
}

pub(crate) async fn resolve_transition(
	state: &ApiState,
	player: Uuid,
	_updates: &OwnedMutexGuard<()>,
) {
	refresh_player(
		&state.realtime,
		player,
		Discovery::Gained,
		catalog(state),
		ready(()),
	)
	.await;
}

pub(crate) async fn deliver_discoveries(state: &ApiState, player: Uuid, player_id: i32) {
	match catalog(state).await {
		Some(catalog) => deliver_with(state, catalog, player, player_id).await,
		None => state.realtime.bonus_retries.mark(player),
	}
}

async fn deliver_with(state: &ApiState, catalog: Arc<SetCatalog>, player: Uuid, player_id: i32) {
	DiscoveryDelivery {
		realtime: state.realtime.clone(),
		catalog,
		player,
		ledger: StoredDiscoveries {
			database: state.database.clone(),
			player_id,
		},
		patience: RECEIPT_PATIENCE,
	}
	.deliver()
	.await;
}

async fn tracked_players(state: &ApiState) -> Vec<Uuid> {
	state
		.realtime
		.player_runtime
		.read()
		.await
		.keys()
		.copied()
		.collect()
}

pub(crate) async fn sets_changed(state: &ApiState) {
	state.bonus_catalog.cache.invalidate_all();
	state.bonus_catalog.changed.notify_one();
	let players = tracked_players(state).await;
	let state = state.clone();
	tokio::spawn(async move {
		rescan_bonuses(&state, players, Discovery::Gained).await;
	});
}

fn wait_until(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Duration {
	at.map_or(WINDOW_RECHECK, |at| {
		(at - now).to_std().unwrap_or_default().min(WINDOW_RECHECK)
	})
}

#[derive(Debug, PartialEq, Eq)]
enum WindowCheck {
	Unavailable,
	Crossed,
	Quiet,
}

fn check_windows(
	catalog: Option<&SetCatalog>,
	processed: DateTime<Utc>,
	now: DateTime<Utc>,
) -> WindowCheck {
	match catalog {
		None => WindowCheck::Unavailable,
		Some(catalog) if catalog.crossed_boundary(processed, now) => WindowCheck::Crossed,
		Some(_) => WindowCheck::Quiet,
	}
}

fn spawn_recorded_delivery(state: &ApiState) {
	let recorded = state
		.bonus_catalog
		.recorded_rx
		.lock()
		.unwrap_or_else(PoisonError::into_inner)
		.take();
	let Some(mut recorded) = recorded else {
		return;
	};
	let state = state.clone();
	tokio::spawn(async move {
		while let Some((player, player_id)) = recorded.recv().await {
			deliver_discoveries(&state, player, player_id).await;
		}
	});
}

async fn wait_for_work(realtime: &RealtimeState, changed: &Notify, wait: Duration) {
	let mut deadline = Instant::now() + wait;
	loop {
		tokio::select! {
			() = sleep_until(deadline) => return,
			() = changed.notified() => return,
			() = realtime.bonus_retries.marked.notified() => {
				deadline = deadline.min(Instant::now() + WINDOW_RETRY);
			}
		}
	}
}

pub(crate) fn spawn_window_refresh(state: ApiState) {
	spawn_recorded_delivery(&state);
	tokio::spawn(async move {
		let mut processed = Utc::now();
		loop {
			let now = Utc::now();
			let catalog = catalog(&state).await;
			let caught_up = match check_windows(catalog.as_deref(), processed, now) {
				WindowCheck::Unavailable => false,
				WindowCheck::Quiet => true,
				WindowCheck::Crossed => {
					let players = tracked_players(&state).await;
					rescan_bonuses(&state, players, Discovery::Gained).await;
					true
				}
			};
			if catalog.is_some() {
				let retries = state.realtime.bonus_retries.take();
				if !retries.is_empty() {
					rescan_bonuses(&state, retries, Discovery::Everything).await;
				}
			}
			let mut wait = if caught_up {
				processed = now;
				let next = catalog.and_then(|catalog| catalog.next_boundary(now));
				wait_until(next, Utc::now())
			} else {
				WINDOW_RETRY
			};
			if !state.realtime.bonus_retries.is_empty() {
				wait = wait.min(WINDOW_RETRY);
			}
			wait_for_work(&state.realtime, &state.bonus_catalog.changed, wait).await;
		}
	});
}

trait DiscoveryLedger: Clone + Send + Sync + 'static {
	fn pending(&self) -> impl Future<Output = Result<Vec<(i32, Vec<i32>)>, DbErr>> + Send;
	fn mark_delivered(&self, set_id: i32) -> impl Future<Output = Result<(), DbErr>> + Send;
}

#[derive(Clone)]
struct StoredDiscoveries {
	database: DatabaseConnection,
	player_id: i32,
}

impl DiscoveryLedger for StoredDiscoveries {
	async fn pending(&self) -> Result<Vec<(i32, Vec<i32>)>, DbErr> {
		Ok(pending_discoveries(self.player_id)
			.all(&self.database)
			.await?
			.into_iter()
			.map(|row| (row.set_id, row.cosmetic_ids))
			.collect())
	}

	async fn mark_delivered(&self, set_id: i32) -> Result<(), DbErr> {
		mark_delivered(self.player_id, vec![set_id])
			.exec(&self.database)
			.await
			.map(drop)
	}
}

#[derive(Debug, Default)]
pub struct DiscoveryWrites {
	queued: HashMap<(Uuid, i32), (Uuid, Instant)>,
	waiting: HashMap<(Uuid, i32), Option<oneshot::Sender<()>>>,
}

fn lock_writes(writes: &StdMutex<DiscoveryWrites>) -> MutexGuard<'_, DiscoveryWrites> {
	writes.lock().unwrap_or_else(PoisonError::into_inner)
}

fn writes(realtime: &RealtimeState) -> MutexGuard<'_, DiscoveryWrites> {
	lock_writes(&realtime.discovery_writes)
}

type Sent = (i32, Vec<i32>, oneshot::Receiver<()>);

#[derive(Clone)]
struct DiscoveryDelivery<L> {
	realtime: RealtimeState,
	catalog: Arc<SetCatalog>,
	player: Uuid,
	ledger: L,
	patience: Duration,
}

impl<L: DiscoveryLedger> DiscoveryDelivery<L> {
	fn deliver(self) -> BoxFuture<'static, ()> {
		Box::pin(async move {
			let sent = {
				let _delivery = self.realtime.discovery_deliveries.lock(self.player).await;
				let pending = match self.ledger.pending().await {
					Ok(pending) => pending,
					Err(error) => {
						warn!("Unable to deliver set discoveries: {error}");
						self.realtime.bonus_retries.mark(self.player);
						return;
					}
				};
				send_discoveries(&self.realtime, &self.catalog, self.player, pending).await
			};
			self.settle_all(sent);
		})
	}

	fn resend(self, bonus_id: i32, cosmetic_ids: Vec<i32>) -> BoxFuture<'static, ()> {
		Box::pin(async move {
			let sent = {
				let _delivery = self.realtime.discovery_deliveries.lock(self.player).await;
				send_discoveries(
					&self.realtime,
					&self.catalog,
					self.player,
					vec![(bonus_id, cosmetic_ids)],
				)
				.await
			};
			self.settle_all(sent);
		})
	}

	fn settle_all(&self, sent: Vec<Sent>) {
		for (bonus_id, cosmetic_ids, receipt) in sent {
			tokio::spawn(self.clone().settle(bonus_id, cosmetic_ids, receipt));
		}
	}

	async fn settle(self, bonus_id: i32, cosmetic_ids: Vec<i32>, receipt: oneshot::Receiver<()>) {
		let written = settle_receipt(
			&self.realtime,
			self.player,
			bonus_id,
			receipt,
			self.patience,
			|set_id| self.ledger.mark_delivered(set_id),
		)
		.await;
		if !written && owner_connected(&self.realtime, self.player).await {
			self.resend(bonus_id, cosmetic_ids).await;
		}
	}
}

async fn owner_connected(realtime: &RealtimeState, player: Uuid) -> bool {
	let connection_ids = realtime
		.connections_by_owner
		.read()
		.await
		.get(&player)
		.cloned()
		.unwrap_or_default();
	let connections = realtime.connections.read().await;
	connection_ids.iter().any(|connection_id| {
		connections
			.get(connection_id)
			.is_some_and(RealtimeConnection::acts_as_owner)
	})
}

fn await_write(
	realtime: &RealtimeState,
	player: Uuid,
	bonus_id: i32,
) -> Option<oneshot::Receiver<()>> {
	let mut writes = writes(realtime);
	if writes.waiting.contains_key(&(player, bonus_id)) {
		return None;
	}
	let (written, receipt) = oneshot::channel();
	writes.waiting.insert((player, bonus_id), Some(written));
	Some(receipt)
}

fn stop_waiting(realtime: &RealtimeState, player: Uuid, bonus_id: i32) {
	writes(realtime).waiting.remove(&(player, bonus_id));
}

fn queue_write(
	realtime: &RealtimeState,
	connection_id: Uuid,
	player: Uuid,
	bonus_id: i32,
) -> Option<WriteReceipt> {
	{
		let mut writes = writes(realtime);
		if writes.queued.contains_key(&(connection_id, bonus_id)) {
			return None;
		}
		writes
			.queued
			.insert((connection_id, bonus_id), (player, Instant::now()));
	}
	let writes = realtime.discovery_writes.clone();
	Some(WriteReceipt::on_settled(move |written| {
		let mut writes = lock_writes(&writes);
		writes.queued.remove(&(connection_id, bonus_id));
		let still_queued = writes
			.queued
			.iter()
			.any(|((_, queued), (owner, _))| *queued == bonus_id && *owner == player);
		if let Some(waiter) = writes.waiting.get_mut(&(player, bonus_id)) {
			if written {
				if let Some(waiter) = waiter.take() {
					let _ = waiter.send(());
				}
			} else if !still_queued {
				drop(waiter.take());
			}
		}
	}))
}

fn unqueue_write(realtime: &RealtimeState, connection_id: Uuid, bonus_id: i32) {
	writes(realtime).queued.remove(&(connection_id, bonus_id));
}

fn retire_stalled(realtime: &RealtimeState, player: Uuid, bonus_id: i32, patience: Duration) {
	let stalled: Vec<Uuid> = writes(realtime)
		.queued
		.iter()
		.filter(|((_, queued), (owner, at))| {
			*queued == bonus_id && *owner == player && at.elapsed() >= patience
		})
		.map(|((connection_id, _), _)| *connection_id)
		.collect();
	for connection_id in stalled {
		realtime.socket_retirements.retire(connection_id);
	}
}

async fn offer_discovery(
	realtime: &RealtimeState,
	player: Uuid,
	bonus_id: i32,
	name: &str,
	cosmetic_ids: &[i32],
) -> bool {
	let connection_ids: Vec<Uuid> = realtime
		.connections_by_owner
		.read()
		.await
		.get(&player)
		.map(|connections| connections.iter().copied().collect())
		.unwrap_or_default();
	let connections = realtime.connections.read().await;
	let mut reached = false;
	for connection_id in connection_ids {
		let Some(connection) = connections.get(&connection_id) else {
			continue;
		};
		if !connection.acts_as_owner() || connection.tx.is_closed() {
			continue;
		}
		let Some(written) = queue_write(realtime, connection_id, player, bonus_id) else {
			reached = true;
			continue;
		};
		let packet = ClientBoundPacket::BonusDiscovered {
			bonus_id,
			name: name.to_owned(),
			cosmetic_ids: cosmetic_ids.to_vec(),
			written,
		};
		match connection.tx.send(packet) {
			Ok(()) => reached = true,
			Err(mpsc::error::SendError(mut packet)) => {
				packet.take_write_receipt().forget();
				unqueue_write(realtime, connection_id, bonus_id);
			}
		}
	}
	reached
}

async fn send_discoveries(
	realtime: &RealtimeState,
	catalog: &SetCatalog,
	player: Uuid,
	pending: Vec<(i32, Vec<i32>)>,
) -> Vec<Sent> {
	let mut sent = Vec::new();
	for (bonus_id, cosmetic_ids) in pending {
		let Some(name) = catalog.names.get(&bonus_id) else {
			continue;
		};
		let Some(receipt) = await_write(realtime, player, bonus_id) else {
			continue;
		};
		if offer_discovery(realtime, player, bonus_id, name, &cosmetic_ids).await {
			sent.push((bonus_id, cosmetic_ids, receipt));
		} else {
			stop_waiting(realtime, player, bonus_id);
		}
	}
	sent
}

async fn settle_receipt<F, E: Display>(
	realtime: &RealtimeState,
	player: Uuid,
	bonus_id: i32,
	receipt: oneshot::Receiver<()>,
	patience: Duration,
	mark_delivered: impl FnOnce(i32) -> F,
) -> bool
where
	F: Future<Output = Result<(), E>>,
{
	let written = matches!(timeout(patience, receipt).await, Ok(Ok(())));
	if written {
		let _delivery = realtime.discovery_deliveries.lock(player).await;
		if let Err(error) = mark_delivered(bonus_id).await {
			warn!("Unable to mark a set discovery delivered: {error}");
		}
		stop_waiting(realtime, player, bonus_id);
	} else {
		stop_waiting(realtime, player, bonus_id);
		retire_stalled(realtime, player, bonus_id, patience);
	}
	written
}

fn equipped_cosmetics(player_id: i32) -> Select<PlayerEquippedCosmetic> {
	PlayerEquippedCosmetic::find()
		.filter(player_equipped_cosmetic::Column::PlayerId.eq(player_id))
}

fn discovery_insert(
	player_id: i32,
	found: &[(i32, Vec<i32>)],
) -> TryInsert<player_set_discovery::ActiveModel> {
	PlayerSetDiscovery::insert_many(found.iter().map(|(set_id, cosmetic_ids)| {
		player_set_discovery::ActiveModel {
			player_id: Set(player_id),
			set_id: Set(*set_id),
			cosmetic_ids: Set(cosmetic_ids.clone()),
			discovered_at: NotSet,
			delivered_at: NotSet,
		}
	}))
	.on_conflict_do_nothing()
}

fn pending_discoveries(player_id: i32) -> Select<PlayerSetDiscovery> {
	PlayerSetDiscovery::find()
		.filter(player_set_discovery::Column::PlayerId.eq(player_id))
		.filter(player_set_discovery::Column::DeliveredAt.is_null())
		.order_by_asc(player_set_discovery::Column::DiscoveredAt)
		.order_by_asc(player_set_discovery::Column::SetId)
}

fn mark_delivered(player_id: i32, set_ids: Vec<i32>) -> UpdateMany<PlayerSetDiscovery> {
	PlayerSetDiscovery::update_many()
		.col_expr(
			player_set_discovery::Column::DeliveredAt,
			Expr::current_timestamp().into(),
		)
		.filter(player_set_discovery::Column::PlayerId.eq(player_id))
		.filter(player_set_discovery::Column::SetId.is_in(set_ids))
		.filter(player_set_discovery::Column::DeliveredAt.is_null())
}

fn set_rule(set: &cosmetic_set::Model, requirements: Vec<Requirement>) -> SetRule {
	SetRule {
		id: set.id,
		priority: set.priority,
		active_from: set.active_from.map(|at| at.with_timezone(&Utc)),
		active_until: set.active_until.map(|at| at.with_timezone(&Utc)),
		requirements,
	}
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{HashMap, HashSet},
		future::{Future, ready},
		sync::{
			Arc, Mutex,
			atomic::{AtomicBool, AtomicUsize, Ordering},
		},
		time::Duration as StdDuration,
	};

	use chrono::{Duration, Utc};
	use entities::sea_orm_active_enums::BodySlot;
	use sea_orm::{DbBackend, DbErr, QueryTrait};
	use tokio::{
		sync::{Notify, mpsc, oneshot},
		task::yield_now,
	};
	use uuid::Uuid;

	use super::{
		BonusRef, CompletionStore, Discovery, DiscoveryDelivery, DiscoveryLedger, Requirement,
		SetCatalog, SetRule, WINDOW_RECHECK, WindowCheck, check_windows, discovery_insert,
		equipped_cosmetics, locked_sets, mark_delivered, pending_discoveries, plan_refresh,
		record_completed, refresh_player, send_discoveries, settle_receipt, wait_for_work,
		wait_until,
	};
	use crate::api::{
		state::{PlayerRuntimeState, RealtimeConnection, RealtimeState},
		v0::{
			account::{ClientKind, TokenAccess},
			websocket::structs::ClientBoundPacket,
		},
	};

	type Rx = mpsc::UnboundedReceiver<ClientBoundPacket>;

	fn set(id: i32, requirements: Vec<Requirement>) -> SetRule {
		SetRule { id, priority: 0, active_from: None, active_until: None, requirements }
	}

	fn group() -> Requirement {
		Requirement::Group { group_id: 4, variant_key: None }
	}

	fn effect(id: i32, hash: &str) -> BonusRef {
		BonusRef { id, url: Some(format!("https://objects.example/effects/{hash}.zip")), hash: hash.to_owned() }
	}

	fn catalog() -> SetCatalog {
		SetCatalog {
			sets: vec![
				set(1, vec![Requirement::Cosmetic(10), group()]),
				set(2, vec![Requirement::Cosmetic(99)]),
			],
			names: HashMap::from([(1, "Sample".to_owned())]),
			refs: HashMap::from([(1, effect(1, "a"))]),
			variants: HashMap::from([(40, (4, None)), (41, (4, None))]),
		}
	}

	fn three_sets(hash: &str) -> SetCatalog {
		SetCatalog {
			sets: (1..=3).map(|id| set(id, vec![Requirement::Cosmetic(10)])).collect(),
			names: HashMap::new(),
			refs: (1..=3).map(|id| (id, effect(id, hash))).collect(),
			variants: HashMap::new(),
		}
	}

	fn wearing(cosmetic_id: i32) -> PlayerRuntimeState {
		PlayerRuntimeState { equipped: HashMap::from([(BodySlot::Hat, cosmetic_id)]), ..Default::default() }
	}

	#[test]
	fn the_catalog_resolves_sets_through_groups_and_windows() {
		let now = Utc::now();
		let catalog = catalog();
		assert_eq!(catalog.completions([10, 41], now), vec![(1, vec![10, 41])]);
		assert_eq!(catalog.active([41, 10], now), vec![effect(1, "a")]);
		assert!(catalog.completions([10], now).is_empty() && catalog.active([10], now).is_empty());
		assert_eq!(three_sets("a").active([10], now), vec![effect(1, "a"), effect(2, "a")]);

		let twice = SetCatalog { sets: vec![set(3, vec![group(), group()])], ..catalog };
		assert!(twice.completions([40, 40], now).is_empty());
		assert!(twice.resolve_items([40, 40], now).is_empty());
		assert_eq!(twice.resolve_items([40, 41, 40], now), vec![3]);

		let hours = |hours: i64| now + Duration::hours(hours);
		let window = |id, from: Option<i64>, until: Option<i64>| SetRule {
			active_from: from.map(hours),
			active_until: until.map(hours),
			..set(id, vec![Requirement::Cosmetic(10)])
		};
		let windows = SetCatalog {
			sets: vec![window(1, Some(-5), Some(9)), window(2, Some(3), None), window(3, None, Some(-1))],
			..SetCatalog::default()
		};
		assert_eq!(windows.next_boundary(now), Some(hours(3)));
		assert_eq!(windows.next_boundary(hours(3)), Some(hours(9)));
		assert_eq!(windows.next_boundary(hours(9)), None);
		assert_eq!(SetCatalog::default().next_boundary(now), None);

		let seconds = |seconds| now + Duration::seconds(seconds);
		let mut opening = three_sets("a");
		opening.sets[0].active_from = Some(seconds(1));
		assert_eq!(check_windows(Some(&opening), now, seconds(2)), WindowCheck::Crossed);
		assert_eq!(check_windows(Some(&opening), seconds(2), seconds(2)), WindowCheck::Quiet);
		assert_eq!(check_windows(None, now, seconds(2)), WindowCheck::Unavailable);
		assert_eq!(check_windows(Some(&opening), now, seconds(40)), WindowCheck::Crossed);

		assert_eq!(wait_until(Some(seconds(30)), now), StdDuration::from_secs(30));
		assert_eq!(wait_until(Some(seconds(-1)), now), StdDuration::ZERO);
		assert_eq!(wait_until(Some(now + Duration::days(9)), now), WINDOW_RECHECK);
		assert_eq!(wait_until(None, now), WINDOW_RECHECK);
	}

	#[test]
	fn a_refresh_follows_windows_and_replaced_bundles() {
		let opens = Utc::now() + Duration::hours(1);
		let mut catalog = three_sets("a");
		catalog.sets[0].active_from = Some(opens);
		catalog.sets[1].active_until = Some(opens);
		let mut runtime = wearing(10);
		let mut refresh = |catalog: &SetCatalog, now| {
			plan_refresh(catalog, Uuid::nil(), &mut runtime, Discovery::Gained, now)
		};
		refresh(&catalog, opens - Duration::seconds(1));

		let change = refresh(&catalog, opens).expect("the windows moved");
		assert!(change.changed);
		assert_eq!(change.bonuses, vec![effect(1, "a"), effect(3, "a")]);
		assert_eq!(catalog.completions([10], opens), vec![(1, vec![10]), (3, vec![10])]);

		catalog.refs.insert(1, effect(1, "b"));
		let change = refresh(&catalog, opens).expect("a new bundle is a change");
		assert!(change.changed);
		assert_eq!(change.bonuses, vec![effect(1, "b"), effect(3, "a")]);
		assert_eq!(refresh(&catalog, opens), None);
		assert_eq!(runtime.bonuses, vec![effect(1, "b"), effect(3, "a")]);
	}

	#[derive(Default)]
	struct Store {
		equipped: Mutex<Vec<i32>>,
		recorded: Mutex<Vec<(i32, Vec<i32>)>>,
		rules: Option<SetCatalog>,
		live: Option<SetCatalog>,
		locked: Mutex<Vec<Vec<i32>>>,
		down: AtomicBool,
	}

	impl Store {
		fn recorded(&self) -> Vec<(i32, Vec<i32>)> {
			self.recorded.lock().unwrap().clone()
		}

		async fn change(&self, cached: Option<&SetCatalog>, wearing: &[i32]) -> Result<bool, DbErr> {
			*self.equipped.lock().unwrap() = wearing.to_vec();
			record_completed(self, cached.cloned().map(Arc::new), 7, Utc::now()).await
		}
	}

	impl CompletionStore for Store {
		async fn rules(&self) -> Result<SetCatalog, DbErr> {
			Ok(self.rules.clone().unwrap_or_default())
		}

		async fn equipped(&self, _player_id: i32) -> Result<Vec<i32>, DbErr> {
			Ok(self.equipped.lock().unwrap().clone())
		}

		async fn current(&self, set_ids: Vec<i32>) -> Result<SetCatalog, DbErr> {
			self.locked.lock().unwrap().push(set_ids.clone());
			let mut live = self.live.clone().or_else(|| self.rules.clone()).unwrap_or_default();
			live.sets.retain(|set| set_ids.contains(&set.id));
			Ok(live)
		}

		async fn record(&self, _player_id: i32, found: Vec<(i32, Vec<i32>)>) -> Result<bool, DbErr> {
			if self.down.load(Ordering::SeqCst) {
				return Err(DbErr::Custom("the database is down".to_owned()));
			}
			let mut recorded = self.recorded.lock().unwrap();
			let before = recorded.len();
			for (set_id, cosmetic_ids) in found {
				if recorded.iter().all(|(recorded, _)| *recorded != set_id) {
					recorded.push((set_id, cosmetic_ids));
				}
			}
			Ok(recorded.len() > before)
		}
	}

	#[tokio::test]
	async fn completed_sets_are_recorded_once_against_their_current_rules() {
		let mut edited = catalog();
		edited.sets[0].requirements = vec![Requirement::Cosmetic(10), Requirement::Cosmetic(41)];
		let mut without_two = three_sets("a");
		without_two.sets.retain(|set| set.id != 2);
		let all = |ids: &[i32]| ids.iter().map(|id| (*id, vec![10])).collect::<Vec<_>>();
		let (three, sample) = (Some(three_sets("a")), Some(catalog()));
		let serving = |live| Store { live, ..Default::default() };
		let ruled = |rules| Store { rules, ..Default::default() };
		type Steps = &'static [(&'static [i32], bool)];
		let cases: Vec<(Store, Option<SetCatalog>, Steps, Vec<(i32, Vec<i32>)>, Option<Vec<Vec<i32>>>)> = vec![
			(serving(three.clone()), three.clone(), &[(&[10], true), (&[10], false)], all(&[1, 2, 3]), None),
			(Store { recorded: Mutex::new(all(&[1, 2])), ..serving(three.clone()) }, three.clone(), &[(&[10], true)], all(&[1, 2, 3]), None),
			(serving(sample.clone()), sample.clone(), &[(&[10, 41], true), (&[10, 40], false), (&[10, 41], false)], vec![(1, vec![10, 41])], None),
			(serving(sample.clone()), sample.clone(), &[(&[10, 40], true), (&[40], false)], vec![(1, vec![10, 40])], None),
			(ruled(sample.clone()), None, &[(&[41, 10], true)], vec![(1, vec![10, 41])], None),
			(ruled(sample.clone()), None, &[(&[10], false)], Vec::new(), None),
			(serving(Some(edited)), sample.clone(), &[(&[10, 40], false), (&[10, 41], true)], vec![(1, vec![10, 41])], Some(vec![vec![1], vec![1]])),
			(serving(Some(without_two)), three.clone(), &[(&[10], true)], all(&[1, 3]), Some(vec![vec![1, 2, 3]])),
			(serving(Some(SetCatalog::default())), three.clone(), &[(&[10], false)], Vec::new(), None),
		];
		for (case, (store, cached, steps, recorded, locked)) in cases.into_iter().enumerate() {
			for (wearing, inserted) in steps {
				assert_eq!(store.change(cached.as_ref(), wearing).await.unwrap(), *inserted, "case {case}");
			}
			assert_eq!(store.recorded(), recorded, "case {case}");
			if let Some(locked) = locked {
				assert_eq!(*store.locked.lock().unwrap(), locked, "case {case}");
			}
		}

		let store = Store { down: AtomicBool::new(true), ..serving(three.clone()) };
		assert!(store.change(three.as_ref(), &[10]).await.is_err());
		assert!(store.recorded().is_empty());
		store.down.store(false, Ordering::SeqCst);
		assert!(store.change(three.as_ref(), &[10]).await.unwrap());
		assert_eq!(store.recorded().len(), 3);
	}

	#[test]
	fn discovery_queries_lock_and_filter_the_right_rows() {
		for (sql, starts, parts, ends) in [
			(locked_sets(vec![3, 1]).build(DbBackend::Postgres).to_string(), "SELECT", &[r#"WHERE "cosmetic_set"."id" IN (3, 1)"#][..], r#"ORDER BY "cosmetic_set"."id" ASC FOR SHARE"#),
			(equipped_cosmetics(7).build(DbBackend::Postgres).to_string(), r#"SELECT "player_equipped_cosmetic"."player_id""#, &[], r#"WHERE "player_equipped_cosmetic"."player_id" = 7"#),
			(
				discovery_insert(7, &[(1, vec![10, 41]), (2, vec![99])]).build(DbBackend::Postgres).to_string(),
				r#"INSERT INTO "player_set_discovery""#,
				&["ARRAY [10,41]"],
				r#"ON CONFLICT ("player_id", "set_id") DO NOTHING"#,
			),
			(
				pending_discoveries(7).build(DbBackend::Postgres).to_string(),
				"SELECT",
				&[r#""player_set_discovery"."player_id" = 7"#, r#""player_set_discovery"."delivered_at" IS NULL"#],
				r#"ORDER BY "player_set_discovery"."discovered_at" ASC, "player_set_discovery"."set_id" ASC"#,
			),
			(
				mark_delivered(7, vec![1, 3]).build(DbBackend::Postgres).to_string(),
				r#"UPDATE "player_set_discovery" SET "delivered_at" = CURRENT_TIMESTAMP"#,
				&[r#""player_set_discovery"."player_id" = 7"#, r#""player_set_discovery"."set_id" IN (1, 3)"#],
				r#""player_set_discovery"."delivered_at" IS NULL"#,
			),
		] {
			assert!(sql.starts_with(starts) && sql.ends_with(ends), "{sql}");
			assert!(parts.iter().all(|part| sql.contains(part)), "{sql}");
			assert!(!(sql.starts_with("INSERT") && sql.contains("delivered_at")), "{sql}");
		}
	}

	async fn connect_as(rt: &RealtimeState, owner: Uuid, access: TokenAccess, watching: &[Uuid]) -> (Rx, Arc<Notify>) {
		let (tx, rx) = mpsc::unbounded_channel();
		let id = Uuid::new_v4();
		let subscriptions = watching.iter().copied().collect::<HashSet<_>>();
		let kind = ClientKind::Game;
		rt.connections.write().await.insert(id, RealtimeConnection { owner, tx, subscriptions, kind, access });
		rt.connections_by_owner.write().await.entry(owner).or_default().insert(id);
		for player in watching {
			rt.watchers.write().await.entry(*player).or_default().insert(id);
		}
		(rx, rt.socket_retirements.watch(id))
	}

	async fn connect(rt: &RealtimeState, player: Uuid) -> Rx {
		connect_as(rt, player, TokenAccess::Player, &[]).await.0
	}

	fn refresh(
		rt: &RealtimeState,
		player: Uuid,
		catalog: Option<SetCatalog>,
		discovery: Discovery,
		hook: impl Future<Output = ()>,
	) -> impl Future<Output = bool> {
		let catalog = ready(catalog.map(Arc::new));
		let refreshed = refresh_player(rt, player, discovery, catalog, hook);
		async move { refreshed.await.is_some() }
	}

	#[tokio::test]
	async fn bonuses_follow_the_live_equipment_and_a_failed_catalogue_retries() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		rt.player_runtime.write().await.insert(player, wearing(10));
		let bonuses = async || rt.player_runtime.read().await[&player].bonuses.clone();

		assert!(!refresh(&rt, player, None, Discovery::Gained, ready(())).await);
		assert!(bonuses().await.is_empty());
		assert_eq!(rt.bonus_retries.take(), vec![player]);
		assert!(rt.bonus_retries.is_empty());

		assert!(refresh(&rt, player, Some(three_sets("a")), Discovery::Everything, ready(())).await);
		assert_eq!(bonuses().await, vec![effect(1, "a"), effect(2, "a")]);
		assert!(rt.bonus_retries.is_empty());

		rt.player_runtime.write().await.get_mut(&player).unwrap().equipped.clear();
		refresh(&rt, player, Some(three_sets("a")), Discovery::Gained, ready(())).await;
		assert!(bonuses().await.is_empty());
	}

	#[tokio::test]
	async fn a_retry_mark_wakes_the_worker_no_sooner_than_the_retry_delay() {
		let rt = RealtimeState::default();
		let changed = Notify::new();
		rt.bonus_retries.mark(Uuid::new_v4());
		let hour = StdDuration::from_secs(3600);
		let within = StdDuration::from_millis(50);

		assert!(tokio::time::timeout(within, wait_for_work(&rt, &changed, hour)).await.is_err());
		changed.notify_one();
		assert!(tokio::time::timeout(within, wait_for_work(&rt, &changed, hour)).await.is_ok());
	}

	#[tokio::test]
	async fn a_refresh_waiting_to_announce_holds_back_a_newer_one() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		rt.player_runtime.write().await.insert(player, wearing(10));
		let (mut rx, _) = connect_as(&rt, Uuid::new_v4(), TokenAccess::Player, &[player]).await;

		let (paused, mut on_pause) = oneshot::channel();
		let (resume, on_resume) = oneshot::channel::<()>();
		let hook = async move {
			let _ = paused.send(());
			let _ = on_resume.await;
		};
		let older = tokio::spawn({
			let rt = rt.clone();
			async move { refresh(&rt, player, Some(three_sets("a")), Discovery::Gained, hook).await }
		});
		while on_pause.try_recv().is_err() {
			yield_now().await;
		}
		let newer = tokio::spawn({
			let rt = rt.clone();
			async move { refresh(&rt, player, Some(three_sets("b")), Discovery::Gained, ready(())).await }
		});
		for _ in 0..64 {
			yield_now().await;
		}
		let _ = resume.send(());
		assert!(older.await.unwrap() && newer.await.unwrap());

		let last_seen = std::iter::from_fn(|| rx.try_recv().ok())
			.filter_map(|packet| match packet {
				ClientBoundPacket::PlayerBonusesChanged { bonuses, .. } => Some(bonuses),
				_ => None,
			})
			.last();
		assert_eq!(last_seen, Some(rt.player_runtime.read().await[&player].bonuses.clone()));
		assert_eq!(last_seen, Some(vec![effect(1, "b"), effect(2, "b")]));
	}

	fn write_all(rx: &mut Rx) -> Vec<i32> {
		let mut written = Vec::new();
		while let Ok(mut packet) = rx.try_recv() {
			packet.take_write_receipt().confirm();
			if let ClientBoundPacket::BonusDiscovered { bonus_id, .. } = packet {
				written.push(bonus_id);
			}
		}
		written
	}

	async fn marked_once_settled(rt: &RealtimeState, player: Uuid, sent: Vec<(i32, Vec<i32>, oneshot::Receiver<()>)>) -> Vec<i32> {
		let mut marked = Vec::new();
		for (bonus_id, _, receipt) in sent {
			let patience = StdDuration::from_secs(5);
			let mark = |_| ready(Ok::<_, &str>(()));
			if settle_receipt(rt, player, bonus_id, receipt, patience, mark).await {
				marked.push(bonus_id);
			}
		}
		marked
	}

	fn nothing_in_flight(rt: &RealtimeState) -> bool {
		let writes = super::writes(rt);
		writes.waiting.is_empty() && writes.queued.is_empty()
	}

	#[tokio::test]
	async fn discoveries_reach_only_the_players_own_sockets() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		let catalog = catalog();
		let pending = vec![(1, vec![10, 41])];
		let (mut staff, _) = connect_as(&rt, player, TokenAccess::SpecialChat, &[]).await;
		let (delegate_tx, mut delegate) = mpsc::unbounded_channel();
		rt.delegate_listeners.write().await.entry(player).or_default().insert(Uuid::new_v4(), delegate_tx);

		let sent = send_discoveries(&rt, &catalog, player, pending.clone()).await;
		assert!(marked_once_settled(&rt, player, sent).await.is_empty());

		let mut own = connect(&rt, player).await;
		let sent = send_discoveries(&rt, &catalog, player, pending).await;
		assert_eq!(write_all(&mut own), vec![1]);
		assert_eq!(marked_once_settled(&rt, player, sent).await, vec![1]);
		assert!(staff.try_recv().is_err() && delegate.try_recv().is_err());
	}

	#[tokio::test]
	async fn a_discovery_that_is_never_written_stays_pending_for_the_next_socket() {
		for closed in [true, false] {
			let rt = RealtimeState::default();
			let player = Uuid::new_v4();
			let catalog = catalog();
			let pending = vec![(1, vec![10, 41])];

			let mut lost = connect(&rt, player).await;
			if closed {
				lost.close();
			}
			let sent = send_discoveries(&rt, &catalog, player, pending.clone()).await;
			if !closed {
				drop(lost.try_recv().expect("the discovery should be queued"));
			}
			drop(lost);
			assert!(marked_once_settled(&rt, player, sent).await.is_empty());
			assert!(nothing_in_flight(&rt));

			let mut back = connect(&rt, player).await;
			let sent = send_discoveries(&rt, &catalog, player, pending).await;
			assert_eq!(write_all(&mut back), vec![1]);
			assert_eq!(marked_once_settled(&rt, player, sent).await, vec![1]);
			assert!(nothing_in_flight(&rt));
		}
	}

	#[tokio::test]
	async fn a_discovery_in_flight_is_not_sent_again_and_any_owner_socket_settles_it() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		let catalog = catalog();
		let pending = vec![(1, vec![10, 41])];
		let mut stalled = connect(&rt, player).await;
		let mut own = connect(&rt, player).await;

		let first = send_discoveries(&rt, &catalog, player, pending.clone()).await;
		assert!(send_discoveries(&rt, &catalog, player, pending).await.is_empty());
		assert_eq!(write_all(&mut own), vec![1]);
		assert_eq!(marked_once_settled(&rt, player, first).await, vec![1]);
		assert!(stalled.try_recv().is_ok());
		assert!(nothing_in_flight(&rt));
	}

	#[derive(Clone, Default)]
	struct Ledger {
		rows: Arc<Mutex<Vec<(i32, bool)>>>,
		scans: Arc<AtomicUsize>,
	}

	impl Ledger {
		fn new(set_ids: &[i32]) -> Self {
			let ledger = Self::default();
			ledger.rows.lock().unwrap().extend(set_ids.iter().map(|id| (*id, false)));
			ledger
		}

		fn delivered(&self) -> Vec<i32> {
			let rows = self.rows.lock().unwrap();
			rows.iter().filter(|(_, delivered)| *delivered).map(|(id, _)| *id).collect()
		}
	}

	impl DiscoveryLedger for Ledger {
		async fn pending(&self) -> Result<Vec<(i32, Vec<i32>)>, DbErr> {
			self.scans.fetch_add(1, Ordering::SeqCst);
			let rows = self.rows.lock().unwrap();
			Ok(rows.iter().filter(|(_, delivered)| !delivered).map(|(id, _)| (*id, vec![10, 41])).collect())
		}

		async fn mark_delivered(&self, set_id: i32) -> Result<(), DbErr> {
			for row in self.rows.lock().unwrap().iter_mut().filter(|row| row.0 == set_id) {
				row.1 = true;
			}
			Ok(())
		}
	}

	fn delivery(rt: &RealtimeState, player: Uuid, ledger: &Ledger, patience: StdDuration) -> DiscoveryDelivery<Ledger> {
		let mut catalog = catalog();
		catalog.names.insert(2, "Other".to_owned());
		let (realtime, catalog, ledger) = (rt.clone(), Arc::new(catalog), ledger.clone());
		DiscoveryDelivery { realtime, catalog, player, ledger, patience }
	}

	async fn eventually(mut done: impl FnMut() -> bool) {
		for _ in 0..1000 {
			if done() {
				return;
			}
			tokio::time::sleep(StdDuration::from_millis(5)).await;
		}
		panic!("the delivery never settled");
	}

	async fn written_eventually(rx: &mut Rx) -> Vec<i32> {
		let mut written = Vec::new();
		eventually(|| {
			written.extend(write_all(rx));
			!written.is_empty()
		})
		.await;
		written
	}

	#[tokio::test]
	async fn a_discovery_lost_by_a_replaced_socket_reaches_the_new_one() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		let ledger = Ledger::new(&[1]);
		let delivery = delivery(&rt, player, &ledger, StdDuration::from_secs(30));

		let mut old = connect(&rt, player).await;
		delivery.clone().deliver().await;
		let queued = old.try_recv().expect("the discovery should be queued");
		let mut new = connect(&rt, player).await;
		delivery.clone().deliver().await;
		assert!(new.try_recv().is_err());

		drop((queued, old));
		assert_eq!(written_eventually(&mut new).await, vec![1]);
		eventually(|| ledger.delivered() == vec![1]).await;
		eventually(|| nothing_in_flight(&rt)).await;
	}

	#[tokio::test]
	async fn a_stalled_socket_holds_back_neither_later_discoveries_nor_its_own() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		let ledger = Ledger::new(&[1]);
		let delivery = delivery(&rt, player, &ledger, StdDuration::from_millis(300));

		let mut stalled = connect(&rt, player).await;
		delivery.clone().deliver().await;
		ledger.rows.lock().unwrap().push((2, false));
		let mut own = connect(&rt, player).await;
		delivery.clone().deliver().await;

		assert_eq!(write_all(&mut own), vec![2]);
		eventually(|| ledger.delivered() == vec![2]).await;
		assert_eq!(written_eventually(&mut own).await, vec![1]);
		eventually(|| ledger.delivered() == vec![1, 2]).await;
		assert!(stalled.try_recv().is_ok());
	}

	#[tokio::test]
	async fn a_stalled_socket_is_retired_and_never_queues_a_discovery_twice() {
		let rt = RealtimeState::default();
		let player = Uuid::new_v4();
		let ledger = Ledger::new(&[1, 2]);
		let patience = StdDuration::from_millis(20);
		let delivery = delivery(&rt, player, &ledger, patience);

		let (mut stalled, retired) = connect_as(&rt, player, TokenAccess::Player, &[]).await;
		delivery.clone().deliver().await;
		tokio::time::timeout(StdDuration::from_secs(5), retired.notified()).await.unwrap();
		tokio::time::sleep(patience * 10).await;

		let queued = std::iter::from_fn(|| stalled.try_recv().ok()).filter_map(|packet| match packet {
			ClientBoundPacket::BonusDiscovered { bonus_id, .. } => Some(bonus_id),
			_ => None,
		});
		assert_eq!(queued.collect::<Vec<_>>(), vec![1, 2]);
		assert_eq!(ledger.scans.load(Ordering::SeqCst), 1);
		assert!(ledger.delivered().is_empty());
	}
}
