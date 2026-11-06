use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
	time::{Duration, Instant},
};

use tokio::sync::{Mutex, MutexGuard, RwLock};
use uuid::Uuid;

use super::{
	join::SharedJoinRequests,
	models::{MemberState, VoiceTransport},
	play::ActivePlays,
};

#[derive(Debug, Clone)]
pub(in crate::api) struct PartyRuntime {
	inner: Arc<RwLock<Inner>>,
	// ponytail: one lock for every party, per-party locks if it ever shows contention.
	mutations: Arc<Mutex<()>>,
	booted_at: Instant,
	pub(super) plays: ActivePlays,
	pub(super) joins: SharedJoinRequests,
}

#[derive(Debug, Default)]
struct Inner {
	states: HashMap<Uuid, MemberState>,
	forwarded_at: HashMap<Uuid, Instant>,
	flush_pending: HashSet<Uuid>,
	last_seen: HashMap<Uuid, Instant>,
	party_of: HashMap<Uuid, Uuid>,
	rosters: HashMap<Uuid, Vec<Uuid>>,
	transports: HashMap<Uuid, VoiceTransport>,
}

pub(super) enum Forward {
	Now,
	After(Duration),
	Pending,
}

impl PartyRuntime {
	pub(in crate::api) fn new() -> Self {
		Self {
			inner: Arc::default(),
			mutations: Arc::default(),
			booted_at: Instant::now(),
			plays: ActivePlays::default(),
			joins: SharedJoinRequests::default(),
		}
	}

	pub(super) async fn lock(&self) -> MutexGuard<'_, ()> {
		self.mutations.lock().await
	}

	pub(super) async fn state_of(&self, player: Uuid) -> Option<MemberState> {
		self.inner.read().await.states.get(&player).cloned()
	}

	pub(super) async fn states_of(&self, players: &[Uuid]) -> HashMap<Uuid, MemberState> {
		let inner = self.inner.read().await;
		players
			.iter()
			.filter_map(|player| Some((*player, inner.states.get(player)?.clone())))
			.collect()
	}

	pub(super) async fn set_state(&self, player: Uuid, state: MemberState) {
		self.inner.write().await.states.insert(player, state);
	}

	pub(super) async fn went_offline(&self, player: Uuid, in_party: bool) {
		let mut inner = self.inner.write().await;
		inner.states.remove(&player);
		inner.forwarded_at.remove(&player);
		if in_party {
			inner.last_seen.insert(player, Instant::now());
		}
	}

	pub(super) async fn forget_seen(&self, player: Uuid) {
		self.inner.write().await.last_seen.remove(&player);
	}

	pub(super) async fn offline_for(&self, player: Uuid) -> Duration {
		self.inner
			.read()
			.await
			.last_seen
			.get(&player)
			.copied()
			.unwrap_or(self.booted_at)
			.elapsed()
	}

	pub(super) async fn party_of(&self, player: Uuid) -> Option<Uuid> {
		self.inner.read().await.party_of.get(&player).copied()
	}

	pub(super) async fn roster(&self, party: Uuid) -> Option<Vec<Uuid>> {
		self.inner.read().await.rosters.get(&party).cloned()
	}

	pub(super) async fn sync_roster(
		&self,
		party: Uuid,
		players: Vec<Uuid>,
		transport: VoiceTransport,
	) {
		let mut inner = self.inner.write().await;
		if let Some(previous) = inner.rosters.remove(&party) {
			for player in previous {
				if !players.contains(&player)
					&& inner.party_of.get(&player) == Some(&party)
				{
					inner.party_of.remove(&player);
				}
			}
		}
		for player in &players {
			inner.party_of.insert(*player, party);
		}
		inner.rosters.insert(party, players);
		inner.transports.insert(party, transport);
	}

	pub(super) async fn forget_party(&self, party: Uuid) {
		let mut inner = self.inner.write().await;
		if let Some(players) = inner.rosters.remove(&party) {
			for player in players {
				if inner.party_of.get(&player) == Some(&party) {
					inner.party_of.remove(&player);
				}
			}
		}
		inner.transports.remove(&party);
	}

	pub(super) async fn transport_changed(
		&self,
		party: Uuid,
		transport: VoiceTransport,
	) -> bool {
		self.inner.read().await.transports.get(&party) != Some(&transport)
	}

	pub(super) async fn schedule_forward(
		&self,
		player: Uuid,
		interval: Duration,
	) -> Forward {
		let mut inner = self.inner.write().await;
		let now = Instant::now();
		match inner.forwarded_at.get(&player).copied() {
			Some(at) if now.duration_since(at) < interval => {
				if inner.flush_pending.insert(player) {
					Forward::After(interval - now.duration_since(at))
				} else {
					Forward::Pending
				}
			}
			_ => {
				inner.forwarded_at.insert(player, now);
				Forward::Now
			}
		}
	}

	pub(super) async fn finish_scheduled_forward(&self, player: Uuid) {
		let mut inner = self.inner.write().await;
		inner.flush_pending.remove(&player);
		inner.forwarded_at.insert(player, Instant::now());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn schedule_forward_coalesces_within_the_interval() {
		let runtime = PartyRuntime::new();
		let player = Uuid::new_v4();
		let interval = Duration::from_secs(60);

		assert!(matches!(
			runtime.schedule_forward(player, interval).await,
			Forward::Now
		));
		assert!(matches!(
			runtime.schedule_forward(player, interval).await,
			Forward::After(_)
		));
		assert!(matches!(
			runtime.schedule_forward(player, interval).await,
			Forward::Pending
		));

		runtime.finish_scheduled_forward(player).await;
		assert!(matches!(
			runtime.schedule_forward(player, interval).await,
			Forward::After(_)
		));
	}
}
