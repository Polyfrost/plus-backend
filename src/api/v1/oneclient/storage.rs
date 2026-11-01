use std::{
	collections::HashSet,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, PoisonError},
	time::Duration,
};

use axum::http::{HeaderMap, HeaderValue, header};
use s3::{Bucket, creds::Credentials, error::S3Error};
use tokio::sync::{Semaphore, SemaphorePermit};
use uuid::Uuid;

use super::{
	CONCURRENT_VALIDATIONS, DOWNLOAD_TTL, LARGE_WRITE_BYTES, LARGE_WRITES_PER_WINDOW,
	SyncError, UPLOAD_TTL, WRITE_WINDOW, WRITES_PER_WINDOW,
};
use crate::{api::rate_limit::RateLimiter, commands::ServeArgs};

const BUSY_RETRY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(in crate::api) struct SyncStorage {
	bucket: Arc<Bucket>,
	writes: RateLimiter<i32>,
	large_writes: RateLimiter<i32>,
	validations: Arc<Semaphore>,
	validating: Arc<Mutex<HashSet<i32>>>,
}

pub(super) struct ValidationSlot<'a> {
	_permit: SemaphorePermit<'a>,
	_player: ValidatingPlayer<'a>,
}

struct ValidatingPlayer<'a> {
	players: &'a Mutex<HashSet<i32>>,
	player_id: i32,
}

impl Drop for ValidatingPlayer<'_> {
	fn drop(&mut self) {
		self.players
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.remove(&self.player_id);
	}
}

pub(super) struct TempFile(PathBuf);

impl TempFile {
	pub(super) fn new() -> Self {
		Self(
			std::env::temp_dir()
				.join(format!("oneclient-sync-{}.mrpack", Uuid::now_v7().simple())),
		)
	}

	pub(super) fn path(&self) -> &Path {
		&self.0
	}
}

impl Drop for TempFile {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.0);
	}
}

impl SyncStorage {
	pub(in crate::api) fn connect(args: &ServeArgs) -> Option<Self> {
		let name = args.oneclient_sync_bucket.as_deref()?.trim();
		if name.is_empty() {
			return None;
		}
		assert_ne!(
			name, args.s3_bucket_name,
			"The OneClient sync bucket must be a separate, private bucket"
		);

		let bucket = Bucket::new(
			name,
			s3::Region::Custom {
				region: args.s3_bucket_region.clone(),
				endpoint: args.s3_bucket_endpoint.clone(),
			},
			Credentials::default().expect(
				"Unable to read s3 credentials (https://lib.rs/crates/aws-creds)",
			),
		)
		.expect("Unable to connect to the OneClient sync bucket")
		.with_path_style();

		Some(Self {
			bucket: bucket.into(),
			writes: RateLimiter::new(WRITES_PER_WINDOW, WRITE_WINDOW),
			large_writes: RateLimiter::new(LARGE_WRITES_PER_WINDOW, WRITE_WINDOW),
			validations: Arc::new(Semaphore::new(CONCURRENT_VALIDATIONS)),
			validating: Arc::default(),
		})
	}

	pub(super) async fn check_write(
		&self,
		player_id: i32,
		size: i64,
	) -> Result<(), SyncError> {
		if let Some(wait) = self.writes.check(player_id).await {
			return Err(SyncError::RateLimited(wait));
		}
		if size > LARGE_WRITE_BYTES
			&& let Some(wait) = self.large_writes.check(player_id).await
		{
			return Err(SyncError::RateLimited(wait));
		}
		Ok(())
	}

	pub(super) async fn validation_slot(
		&self,
		player_id: i32,
	) -> Result<ValidationSlot<'_>, SyncError> {
		if !self
			.validating
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.insert(player_id)
		{
			return Err(SyncError::RateLimited(BUSY_RETRY));
		}
		let player = ValidatingPlayer {
			players: &self.validating,
			player_id,
		};
		let permit = self
			.validations
			.acquire()
			.await
			.expect("infallible: the validation semaphore is never closed");
		Ok(ValidationSlot {
			_permit: permit,
			_player: player,
		})
	}

	pub(super) fn staging_key(upload: Uuid) -> String {
		format!("staging/{}.mrpack", upload.simple())
	}

	pub(super) fn payload_key(code: &str, upload: Uuid) -> String {
		format!("payloads/{code}/{}.mrpack", upload.simple())
	}

	pub(super) fn is_payload_of(key: &str, upload: Uuid) -> bool {
		key.ends_with(&format!("/{}.mrpack", upload.simple()))
	}

	pub(super) async fn upload_url(
		&self,
		key: &str,
		size: i64,
	) -> Result<String, SyncError> {
		let mut headers = HeaderMap::new();
		headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
		Ok(self
			.bucket
			.presign_put(key, UPLOAD_TTL.as_secs() as u32, Some(headers), None)
			.await?)
	}

	pub(super) async fn download_url(&self, key: &str) -> Result<String, SyncError> {
		Ok(self
			.bucket
			.presign_get(key, DOWNLOAD_TTL.as_secs() as u32, None)
			.await?)
	}

	pub(super) async fn uploaded_size(
		&self,
		key: &str,
	) -> Result<Option<i64>, SyncError> {
		match self.bucket.head_object(key).await {
			Ok((head, _)) => Ok(head.content_length),
			Err(S3Error::HttpFailWithBody(404, _)) => Ok(None),
			Err(error) => Err(error.into()),
		}
	}

	pub(super) async fn download(&self, key: &str, dest: &Path) -> Result<(), SyncError> {
		let mut file = tokio::fs::File::create(dest).await?;
		self.bucket.get_object_to_writer(key, &mut file).await?;
		file.sync_all().await?;
		Ok(())
	}

	pub(super) async fn store(&self, source: &Path, key: &str) -> Result<(), SyncError> {
		let mut file = tokio::fs::File::open(source).await?;
		self.bucket.put_object_stream(&mut file, key).await?;
		Ok(())
	}

	pub(super) async fn remove(&self, key: &str) -> Result<(), S3Error> {
		match self.bucket.delete_object(key).await {
			Ok(_) | Err(S3Error::HttpFailWithBody(404, _)) => Ok(()),
			Err(error) => Err(error),
		}
	}
}
