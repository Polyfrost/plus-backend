mod payload;
mod records;
mod storage;
mod sweeper;

use std::time::Duration;

use aide::{OperationIo, axum::ApiRouter};
use axum::{
	Json,
	http::{HeaderValue, StatusCode, header},
	response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::DbErr;
use tower_http::set_header::SetResponseHeaderLayer;

pub(in crate::api) use self::{storage::SyncStorage, sweeper::spawn_sync_sweeper};
use crate::api::ApiState;

const MAX_PAYLOAD_BYTES: u64 = 128 * 1024 * 1024;
const MAX_NAME_CHARS: usize = 64;
const MAX_DEVICE_CHARS: usize = 64;
const MAX_VERSION_CHARS: usize = 32;
const MAX_FORMAT: i32 = 1000;
const MAX_LIVE_RECORDS: u64 = 100;
const MAX_ACCOUNT_BYTES: i64 = 512 * 1024 * 1024;
const MAX_TOMBSTONES: u64 = 500;
const LIST_PAGE_SIZE: u64 = 200;
const UPLOAD_TTL: Duration = Duration::from_secs(15 * 60);
const DOWNLOAD_TTL: Duration = Duration::from_secs(10 * 60);
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const WRITE_WINDOW: Duration = Duration::from_secs(60 * 60);
const WRITES_PER_WINDOW: u32 = 240;
const LARGE_WRITE_BYTES: i64 = 16 * 1024 * 1024;
const LARGE_WRITES_PER_WINDOW: u32 = 24;
const CONCURRENT_VALIDATIONS: usize = 4;

pub(super) fn router() -> ApiRouter<ApiState> {
	records::router().layer(SetResponseHeaderLayer::overriding(
		header::CACHE_CONTROL,
		HeaderValue::from_static("no-store"),
	))
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub(in crate::api) enum SyncError {
	#[error("Cloud sync is not available right now")]
	Unavailable,
	#[error("No synced instance with that id exists")]
	NotFound,
	#[error("This instance was removed from the cloud")]
	Gone,
	#[error("The cloud copy changed since the revision you last synced from")]
	Conflict {
		revision: i64,
		updated_by: String,
		updated_at: DateTime<Utc>,
	},
	#[error("Updates must send If-Match with the revision they were made from")]
	PreconditionRequired,
	#[error("The instance is {size} bytes, over the {limit} byte limit")]
	TooLarge { size: i64, limit: u64 },
	#[error("{0}")]
	InvalidRequest(String),
	#[error("{0}")]
	LimitReached(String),
	#[error("The payload was not uploaded before committing")]
	UploadMissing,
	#[error("{0}")]
	InvalidPayload(String),
	#[error("{0}")]
	SecretDetected(String),
	#[error("Too many sync writes, try again later")]
	RateLimited(Duration),
	#[error("Unable to query database: {0}")]
	Database(#[from] DbErr),
	#[error("Unable to reach object storage: {0}")]
	Storage(#[from] s3::error::S3Error),
	#[error("Unable to process the payload: {0}")]
	Io(#[from] std::io::Error),
}

impl SyncError {
	fn status(&self) -> StatusCode {
		match self {
			Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
			Self::NotFound => StatusCode::NOT_FOUND,
			Self::Gone => StatusCode::GONE,
			Self::Conflict { .. } => StatusCode::PRECONDITION_FAILED,
			Self::PreconditionRequired => StatusCode::PRECONDITION_REQUIRED,
			Self::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
			Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
			Self::LimitReached(_) | Self::UploadMissing => StatusCode::CONFLICT,
			Self::InvalidPayload(_) | Self::SecretDetected(_) => {
				StatusCode::UNPROCESSABLE_ENTITY
			}
			Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
			Self::Database(_) | Self::Storage(_) | Self::Io(_) => {
				StatusCode::INTERNAL_SERVER_ERROR
			}
		}
	}

	fn code(&self) -> &'static str {
		match self {
			Self::Unavailable => "unavailable",
			Self::NotFound => "not_found",
			Self::Gone => "gone",
			Self::Conflict { .. } => "conflict",
			Self::PreconditionRequired => "precondition_required",
			Self::TooLarge { .. } => "too_large",
			Self::InvalidRequest(_) => "invalid_request",
			Self::LimitReached(_) => "limit_reached",
			Self::UploadMissing => "upload_missing",
			Self::InvalidPayload(_) => "invalid_payload",
			Self::SecretDetected(_) => "secret_detected",
			Self::RateLimited(_) => "rate_limited",
			Self::Database(_) | Self::Storage(_) | Self::Io(_) => "internal",
		}
	}
}

impl IntoResponse for SyncError {
	fn into_response(self) -> Response {
		let status = self.status();
		let message = if status.is_server_error() && !matches!(self, Self::Unavailable) {
			tracing::error!(%status, error = %self, "oneclient sync request failed");
			crate::api::INTERNAL_MESSAGE.to_string()
		} else {
			self.to_string()
		};

		let mut body = serde_json::json!({ "error": self.code(), "message": message });
		if let Self::Conflict {
			revision,
			updated_by,
			updated_at,
		} = &self
		{
			body["revision"] = (*revision).into();
			body["updated_by"] = updated_by.as_str().into();
			body["updated_at"] = updated_at.to_rfc3339().into();
		}

		let mut response = (status, Json(body)).into_response();
		if let Self::RateLimited(wait) = self
			&& let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string())
		{
			response.headers_mut().insert(header::RETRY_AFTER, value);
		}
		response
	}
}
