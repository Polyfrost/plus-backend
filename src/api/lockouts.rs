use std::time::Duration;

use chrono::{TimeDelta, Utc};
use entities::{prelude::*, rate_limit_lockout};
use sea_orm::{
	ActiveValue::{NotSet, Set},
	ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter,
};
use tokio::{select, sync::mpsc, time};
use tracing::warn;

use crate::api::rate_limit::{LimitKey, RateLimiter};

/// Lockouts are kept this long, to tune the limits against.
const RETENTION: TimeDelta = TimeDelta::days(7);
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const QUEUE_SIZE: usize = 1024;
const BATCH_SIZE: usize = 256;

#[derive(Debug)]
pub(in crate::api) struct Lockout {
	pub(in crate::api) limiter: &'static str,
	pub(in crate::api) key: String,
	pub(in crate::api) timeout: Duration,
	pub(in crate::api) limit: u32,
	pub(in crate::api) window: Duration,
}

/// Writes lockouts to the database in the background. The default log
/// records nothing.
#[derive(Debug, Clone, Default)]
pub(in crate::api) struct LockoutLog(Option<mpsc::Sender<Lockout>>);

impl LockoutLog {
	pub(in crate::api) fn spawn(database: DatabaseConnection) -> Self {
		let (tx, rx) = mpsc::channel(QUEUE_SIZE);
		tokio::spawn(persist(database, rx));
		Self(Some(tx))
	}

	/// A log whose lockouts land in the returned receiver.
	#[cfg(test)]
	pub(in crate::api) fn capture() -> (Self, mpsc::Receiver<Lockout>) {
		let (tx, rx) = mpsc::channel(QUEUE_SIZE);
		(Self(Some(tx)), rx)
	}

	/// A limiter whose lockouts go to this log, under `name`.
	pub(in crate::api) fn limiter<K: LimitKey>(
		&self,
		name: &'static str,
		limit: u32,
		window: Duration,
	) -> RateLimiter<K> {
		RateLimiter::new(name, limit, window, self.clone())
	}

	/// Dropped when the queue is full: losing a few lockouts beats slowing
	/// down the requests being limited.
	pub(in crate::api) fn record(&self, lockout: Lockout) {
		if let Some(tx) = &self.0 {
			let _ = tx.try_send(lockout);
		}
	}
}

async fn persist(database: DatabaseConnection, mut rx: mpsc::Receiver<Lockout>) {
	let mut prune = time::interval(PRUNE_INTERVAL);
	let mut batch = Vec::with_capacity(BATCH_SIZE);

	loop {
		select! {
			_ = prune.tick() => {
				if let Err(error) = prune_expired(&database).await {
					warn!(%error, "Unable to prune rate limit lockouts");
				}
			}
			received = rx.recv_many(&mut batch, BATCH_SIZE) => {
				if received == 0 {
					return;
				}
				if let Err(error) = insert(&database, batch.drain(..)).await {
					warn!(%error, "Unable to record rate limit lockouts");
				}
			}
		}
	}
}

fn seconds(duration: Duration) -> i64 {
	i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

async fn insert(
	database: &DatabaseConnection,
	lockouts: impl Iterator<Item = Lockout>,
) -> Result<(), DbErr> {
	RateLimitLockout::insert_many(lockouts.map(|lockout| {
		rate_limit_lockout::ActiveModel {
			id: NotSet,
			limiter: Set(lockout.limiter.to_owned()),
			key: Set(lockout.key),
			timeout_seconds: Set(seconds(lockout.timeout)),
			request_limit: Set(lockout.limit.into()),
			window_seconds: Set(seconds(lockout.window)),
			created_at: NotSet,
		}
	}))
	.on_empty_do_nothing()
	.exec_without_returning(database)
	.await?;
	Ok(())
}

async fn prune_expired(database: &DatabaseConnection) -> Result<(), DbErr> {
	RateLimitLockout::delete_many()
		.filter(rate_limit_lockout::Column::CreatedAt.lt(Utc::now() - RETENTION))
		.exec(database)
		.await?;
	Ok(())
}
