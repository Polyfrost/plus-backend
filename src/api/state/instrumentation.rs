use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::Duration,
};

use chrono::Utc;
use entities::{analytics_cosmetic_daily, prelude::*};
use sea_orm::{
	ActiveValue, DatabaseConnection, DbErr, EntityTrait,
	sea_query::{Alias, Expr, OnConflict},
};
use tracing::warn;

const FLUSH_INTERVAL: Duration = Duration::from_secs(60);
const VIEW_MAP_CAP: usize = 20_000;

#[derive(Debug, Default)]
struct Buffer {
	views: HashMap<i32, u32>,
	dropped: u64,
}

/// Buffered in memory so a request never waits on an analytics write.
#[derive(Debug, Clone, Default)]
pub struct Instrumentation(Arc<Mutex<Buffer>>);

impl Instrumentation {
	pub fn record_cosmetic_view(&self, cosmetic_id: i32) {
		let Ok(mut buffer) = self.0.lock() else {
			return;
		};

		if buffer.views.len() >= VIEW_MAP_CAP && !buffer.views.contains_key(&cosmetic_id)
		{
			buffer.dropped += 1;
			return;
		}

		*buffer.views.entry(cosmetic_id).or_default() += 1;
	}

	fn take(&self) -> Option<(HashMap<i32, u32>, u64)> {
		let mut buffer = self.0.lock().ok()?;
		if buffer.views.is_empty() {
			return None;
		}

		Some((
			std::mem::take(&mut buffer.views),
			std::mem::take(&mut buffer.dropped),
		))
	}
}

pub(super) fn spawn_instrumentation_flush(
	database: DatabaseConnection,
	instrumentation: Instrumentation,
) {
	tokio::spawn(flush_loop(database, instrumentation));
}

async fn flush_loop(database: DatabaseConnection, instrumentation: Instrumentation) {
	let mut interval = tokio::time::interval(FLUSH_INTERVAL);
	interval.tick().await;

	loop {
		interval.tick().await;

		let Some((views, dropped)) = instrumentation.take() else {
			continue;
		};

		if dropped > 0 {
			warn!(dropped, "Cosmetic view buffer overflowed, counts are short");
		}

		if let Err(error) = flush_views(&database, views).await {
			warn!("Unable to flush cosmetic views: {error}");
		}
	}
}

async fn flush_views(
	database: &DatabaseConnection,
	views: HashMap<i32, u32>,
) -> Result<(), DbErr> {
	let day = Utc::now().date_naive();
	let rows = views.into_iter().map(|(cosmetic_id, count)| {
		analytics_cosmetic_daily::ActiveModel {
			day: ActiveValue::Set(day),
			cosmetic_id: ActiveValue::Set(cosmetic_id),
			views: ActiveValue::Set(i64::from(count)),
			..Default::default()
		}
	});

	AnalyticsCosmeticDaily::insert_many(rows)
		.on_conflict(
			OnConflict::columns([
				analytics_cosmetic_daily::Column::Day,
				analytics_cosmetic_daily::Column::CosmeticId,
			])
			.value(
				analytics_cosmetic_daily::Column::Views,
				Expr::col((
					analytics_cosmetic_daily::Entity,
					analytics_cosmetic_daily::Column::Views,
				))
				.add(Expr::col((
					Alias::new("excluded"),
					analytics_cosmetic_daily::Column::Views,
				))),
			)
			.to_owned(),
		)
		.exec_without_returning(database)
		.await?;

	Ok(())
}
