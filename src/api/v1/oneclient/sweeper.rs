use std::time::Duration;

use chrono::Utc;
use entities::{
	oneclient_sync_garbage, oneclient_sync_record, oneclient_sync_upload, prelude::*,
};
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder,
	QuerySelect, Set, TransactionTrait,
};
use tracing::warn;

use super::{DOWNLOAD_TTL, SyncError, TOMBSTONE_RETENTION};
use crate::api::ApiState;

const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
const UPLOAD_GRACE: Duration = Duration::from_secs(5 * 60);
const UPLOAD_BATCH: u64 = 200;
const GARBAGE_BATCH: u64 = 500;

pub(in crate::api) fn spawn_sync_sweeper(state: ApiState) {
	if state.oneclient_sync.is_none() {
		return;
	}
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(SWEEP_INTERVAL);
		loop {
			interval.tick().await;

			if let Err(error) = expire_uploads(&state).await {
				warn!("Unable to expire OneClient sync uploads: {error}");
			}
			if let Err(error) = purge_tombstones(&state).await {
				warn!("Unable to purge OneClient sync tombstones: {error}");
			}
			if let Err(error) = collect_garbage(&state).await {
				warn!("Unable to delete OneClient sync payloads: {error}");
			}
		}
	});
}

async fn expire_uploads(state: &ApiState) -> Result<(), SyncError> {
	let cutoff = (Utc::now() - UPLOAD_GRACE).fixed_offset();
	let expired = OneclientSyncUpload::find()
		.filter(oneclient_sync_upload::Column::ExpiresAt.lt(cutoff))
		.limit(UPLOAD_BATCH)
		.all(&state.database)
		.await?;

	for upload in expired {
		let txn = state.database.begin().await?;
		let record = OneclientSyncRecord::find_by_id(upload.record_id)
			.lock_exclusive()
			.one(&txn)
			.await?;
		let removed = OneclientSyncUpload::delete_many()
			.filter(oneclient_sync_upload::Column::Id.eq(upload.id))
			.filter(oneclient_sync_upload::Column::ExpiresAt.lt(cutoff))
			.exec(&txn)
			.await?
			.rows_affected;
		if removed > 0 {
			oneclient_sync_garbage::ActiveModel {
				object_key: Set(upload.staging_key),
				..Default::default()
			}
			.insert(&txn)
			.await?;
			if let Some(record) = record.filter(|record| record.revision == 0) {
				record.into_active_model().delete(&txn).await?;
			}
		}
		txn.commit().await?;
	}
	Ok(())
}

async fn purge_tombstones(state: &ApiState) -> Result<(), SyncError> {
	OneclientSyncRecord::delete_many()
		.filter(oneclient_sync_record::Column::Deleted.eq(true))
		.filter(
			oneclient_sync_record::Column::UpdatedAt
				.lt((Utc::now() - TOMBSTONE_RETENTION).fixed_offset()),
		)
		.exec(&state.database)
		.await?;
	Ok(())
}

async fn collect_garbage(state: &ApiState) -> Result<(), SyncError> {
	let Some(storage) = state.oneclient_sync.as_ref() else {
		return Ok(());
	};
	let garbage = OneclientSyncGarbage::find()
		.filter(
			oneclient_sync_garbage::Column::CreatedAt
				.lt((Utc::now() - DOWNLOAD_TTL).fixed_offset()),
		)
		.order_by_asc(oneclient_sync_garbage::Column::Id)
		.limit(GARBAGE_BATCH)
		.all(&state.database)
		.await?;

	let mut removed = Vec::with_capacity(garbage.len());
	for item in garbage {
		match storage.remove(&item.object_key).await {
			Ok(()) => removed.push(item.id),
			Err(error) => {
				warn!(key = %item.object_key, "Unable to delete a sync payload: {error}")
			}
		}
	}
	if !removed.is_empty() {
		OneclientSyncGarbage::delete_many()
			.filter(oneclient_sync_garbage::Column::Id.is_in(removed))
			.exec(&state.database)
			.await?;
	}
	Ok(())
}
