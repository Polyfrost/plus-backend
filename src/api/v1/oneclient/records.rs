use aide::{
	OperationOutput,
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	generate::GenContext,
	openapi::Operation,
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, Query, State},
	http::{HeaderMap, HeaderValue, StatusCode, header},
	response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use entities::{
	oneclient_sync_garbage, oneclient_sync_record, oneclient_sync_upload, prelude::*,
	user,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait,
	IntoActiveModel, QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
	LIST_PAGE_SIZE, MAX_ACCOUNT_BYTES, MAX_DEVICE_CHARS, MAX_FORMAT, MAX_LIVE_RECORDS,
	MAX_NAME_CHARS, MAX_PAYLOAD_BYTES, MAX_TOMBSTONES, MAX_VERSION_CHARS, SyncError,
	UPLOAD_TTL,
	payload::{self, Declared, PayloadError},
	storage::{SyncStorage, TempFile},
};
use crate::api::{ApiState, v0::account::AuthenticatedPlayer};

const CODE_LENGTH: usize = 8;
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_ATTEMPTS: usize = 16;
const DEFAULT_DEVICE: &str = "Unknown device";
const PRIVATE: &str = "private";

#[derive(Debug, Serialize, JsonSchema)]
struct MetaView {
	format: i32,
	name: String,
	mc_version: String,
	mc_loader: String,
	kind: String,
	size: i64,
	sha512: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RecordView {
	id: String,
	revision: i64,
	meta: MetaView,
	provisioned: bool,
	updated_at: DateTime<Utc>,
	updated_by: String,
	deleted: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RecordResponse {
	#[serde(flatten)]
	record: RecordView,
	#[serde(skip_serializing_if = "Option::is_none")]
	download_url: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct UploadTarget {
	id: Uuid,
	url: String,
	method: &'static str,
	headers: std::collections::BTreeMap<&'static str, String>,
	expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReservedResponse {
	#[serde(flatten)]
	record: RecordView,
	upload: UploadTarget,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CommittedResponse {
	#[serde(flatten)]
	record: RecordView,
	removed: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListResponse {
	records: Vec<RecordView>,
	cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UploadRequest {
	format: i32,
	name: String,
	mc_version: String,
	mc_loader: String,
	kind: String,
	size: i64,
	sha512: String,
	#[serde(default)]
	provisioned: bool,
	#[serde(default)]
	device: Option<String>,
	#[serde(default)]
	visibility: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CommitRequest {
	upload_id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListQuery {
	mine: Option<String>,
	visibility: Option<String>,
	since: Option<DateTime<Utc>>,
	cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecordPath {
	id: String,
}

struct Tagged<T>(i64, T);

impl<T: Serialize> IntoResponse for Tagged<T> {
	fn into_response(self) -> Response {
		let mut response = Json(self.1).into_response();
		if let Ok(value) = HeaderValue::from_str(&etag(self.0)) {
			response.headers_mut().insert(header::ETAG, value);
		}
		response
	}
}

impl<T: JsonSchema> OperationOutput for Tagged<T> {
	type Inner = T;

	fn operation_response(
		ctx: &mut GenContext,
		operation: &mut Operation,
	) -> Option<aide::openapi::Response> {
		Json::<T>::operation_response(ctx, operation)
	}

	fn inferred_responses(
		ctx: &mut GenContext,
		operation: &mut Operation,
	) -> Vec<(Option<u16>, aide::openapi::Response)> {
		Json::<T>::inferred_responses(ctx, operation)
	}
}

fn etag(revision: i64) -> String {
	format!("\"r{revision}\"")
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listSyncedInstances")
		.summary("List the player's synced instances")
		.description(
			"Lists every synced OneClient instance of the authenticated player, \
			 including deleted markers kept for 90 days. Requires `mine=1` and \
			 `visibility=private`. Paged by `cursor` past 200 records; `since` \
			 limits the list to records updated after that time.",
		)
		.tag("oneclient")
}

fn get_doc(op: TransformOperation) -> TransformOperation {
	op.id("getSyncedInstance")
		.summary("Get a synced instance")
		.description(
			"Returns one synced instance with a short-lived `download_url`, and its \
			 revision as the `ETag`. Ids of other players answer `404`.",
		)
		.tag("oneclient")
}

fn payload_doc(op: TransformOperation) -> TransformOperation {
	op.id("downloadSyncedInstance")
		.summary("Download a synced instance")
		.description("Redirects to a short-lived signed URL for the current payload.")
		.tag("oneclient")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createSyncedInstance")
		.summary("Start syncing an instance")
		.description(
			"Reserves revision 1 of a new private record and returns a signed upload \
			 target. The upload must carry exactly the declared `size`. The record \
			 stays hidden until `POST /shares/{id}/commit` succeeds.",
		)
		.tag("oneclient")
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateSyncedInstance")
		.summary("Upload a new revision")
		.description(
			"Reserves the next revision. Requires `If-Match: \"r<revision>\"` with the \
			 revision the client last synced from; a stale revision answers `412` \
			 with the current one, and a missing header answers `428`.",
		)
		.tag("oneclient")
}

fn commit_doc(op: TransformOperation) -> TransformOperation {
	op.id("commitSyncedInstance")
		.summary("Commit an uploaded revision")
		.description(
			"Checks the uploaded payload and makes its revision current. Files that \
			 are not synced, and settings that look like they hold a password, token \
			 or key, are removed from the stored copy and listed in `removed`.",
		)
		.tag("oneclient")
}

fn delete_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteSyncedInstance")
		.summary("Stop syncing an instance")
		.description(
			"Deletes the cloud copy and leaves a deleted marker so other computers \
			 stop syncing it. `If-Match` is optional.",
		)
		.tag("oneclient")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The instance is no longer synced")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/shares",
			get_with(self::list, self::list_doc)
				.post_with(self::create, self::create_doc),
		)
		.api_route(
			"/shares/{id}",
			get_with(self::get, self::get_doc)
				.put_with(self::update, self::update_doc)
				.delete_with(self::delete, self::delete_doc),
		)
		.api_route(
			"/shares/{id}/payload",
			get_with(self::payload, self::payload_doc),
		)
		.api_route(
			"/shares/{id}/commit",
			post_with(self::commit, self::commit_doc),
		)
}

fn storage(state: &ApiState) -> Result<&SyncStorage, SyncError> {
	state.oneclient_sync.as_ref().ok_or(SyncError::Unavailable)
}

fn parse_code(raw: &str) -> Result<String, SyncError> {
	let mut code = String::with_capacity(CODE_LENGTH);
	for ch in raw.chars().filter(|ch| *ch != '-') {
		let ch = match ch.to_ascii_uppercase() {
			'O' => '0',
			'I' | 'L' => '1',
			other => other,
		};
		if !ch.is_ascii() || !CODE_ALPHABET.contains(&(ch as u8)) {
			return Err(SyncError::NotFound);
		}
		code.push(ch);
	}
	if code.len() == CODE_LENGTH {
		Ok(code)
	} else {
		Err(SyncError::NotFound)
	}
}

fn generate_code() -> String {
	(0..CODE_LENGTH)
		.map(|_| CODE_ALPHABET[rand::random_range(0..CODE_ALPHABET.len())] as char)
		.collect()
}

fn parse_if_match(headers: &HeaderMap) -> Result<Option<i64>, SyncError> {
	let Some(value) = headers.get(header::IF_MATCH) else {
		return Ok(None);
	};
	value
		.to_str()
		.ok()
		.map(str::trim)
		.and_then(|value| {
			value
				.strip_prefix('"')?
				.strip_suffix('"')?
				.strip_prefix('r')
		})
		.and_then(|revision| revision.parse::<i64>().ok())
		.filter(|revision| *revision > 0)
		.map(Some)
		.ok_or_else(|| {
			SyncError::InvalidRequest("If-Match must look like \"r<revision>\"".into())
		})
}

fn is_plain_text(value: &str, max: usize) -> bool {
	let count = value.chars().count();
	count > 0 && count <= max && !value.chars().any(char::is_control)
}

fn validate(body: &UploadRequest) -> Result<(Declared, bool, String), SyncError> {
	let invalid = |message: &str| Err(SyncError::InvalidRequest(message.to_string()));

	if !(1..=MAX_FORMAT).contains(&body.format) {
		return invalid("Unsupported format");
	}
	if !is_plain_text(&body.name, MAX_NAME_CHARS) {
		return invalid("Names must be 1 to 64 characters without control characters");
	}
	if body.mc_version.is_empty()
		|| body.mc_version.len() > MAX_VERSION_CHARS
		|| !body
			.mc_version
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || "._-+ ".contains(c))
	{
		return invalid("Invalid Minecraft version");
	}
	if !payload::is_known_loader(&body.mc_loader) {
		return invalid("Unknown loader");
	}
	if !payload::is_known_kind(&body.kind) {
		return invalid("Unknown instance kind");
	}
	if body.size <= 0 {
		return invalid("The size must be positive");
	}
	if body.size as u64 > MAX_PAYLOAD_BYTES {
		return Err(SyncError::TooLarge {
			size: body.size,
			limit: MAX_PAYLOAD_BYTES,
		});
	}
	let sha512 = body.sha512.trim().to_ascii_lowercase();
	if sha512.len() != 128 || !sha512.chars().all(|c| c.is_ascii_hexdigit()) {
		return invalid("sha512 must be 128 hexadecimal characters");
	}

	let device = body
		.device
		.as_deref()
		.map(str::trim)
		.filter(|device| is_plain_text(device, MAX_DEVICE_CHARS))
		.unwrap_or(DEFAULT_DEVICE)
		.to_string();

	Ok((
		Declared {
			format: body.format,
			name: body.name.clone(),
			mc_version: body.mc_version.clone(),
			mc_loader: body.mc_loader.clone(),
			kind: body.kind.clone(),
			size: body.size,
			sha512,
		},
		body.provisioned,
		device,
	))
}

fn record_view(record: &oneclient_sync_record::Model) -> RecordView {
	RecordView {
		id: record.code.clone(),
		revision: record.revision,
		meta: MetaView {
			format: record.format,
			name: record.name.clone(),
			mc_version: record.mc_version.clone(),
			mc_loader: record.mc_loader.clone(),
			kind: record.kind.clone(),
			size: record.size,
			sha512: record.sha512.clone(),
		},
		provisioned: record.provisioned,
		updated_at: record.updated_at.with_timezone(&Utc),
		updated_by: record.updated_by.clone(),
		deleted: record.deleted,
	}
}

fn reserved_view(
	record: &oneclient_sync_record::Model,
	upload: &oneclient_sync_upload::Model,
) -> RecordView {
	RecordView {
		id: record.code.clone(),
		revision: upload.revision,
		meta: MetaView {
			format: upload.format,
			name: upload.name.clone(),
			mc_version: upload.mc_version.clone(),
			mc_loader: upload.mc_loader.clone(),
			kind: upload.kind.clone(),
			size: upload.size,
			sha512: upload.sha512.clone(),
		},
		provisioned: upload.provisioned,
		updated_at: Utc::now(),
		updated_by: upload.device.clone(),
		deleted: false,
	}
}

fn conflict(record: &oneclient_sync_record::Model) -> SyncError {
	SyncError::Conflict {
		revision: record.revision,
		updated_by: record.updated_by.clone(),
		updated_at: record.updated_at.with_timezone(&Utc),
	}
}

async fn find_record<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	code: &str,
	lock: bool,
) -> Result<Option<oneclient_sync_record::Model>, SyncError> {
	let mut query = OneclientSyncRecord::find()
		.filter(oneclient_sync_record::Column::PlayerId.eq(player_id))
		.filter(oneclient_sync_record::Column::Code.eq(code));
	if lock {
		query = query.lock_exclusive();
	}
	Ok(query.one(db).await?)
}

async fn find_visible<C: ConnectionTrait>(
	db: &C,
	player_id: i32,
	code: &str,
	lock: bool,
) -> Result<oneclient_sync_record::Model, SyncError> {
	find_record(db, player_id, code, lock)
		.await?
		.filter(|record| record.revision > 0)
		.ok_or(SyncError::NotFound)
}

async fn lock_player(
	txn: &DatabaseTransaction,
	player: &user::Model,
) -> Result<(), SyncError> {
	User::find_by_id(player.id)
		.lock_exclusive()
		.one(txn)
		.await?;
	Ok(())
}

async fn discard<C: ConnectionTrait>(
	db: &C,
	key: Option<String>,
) -> Result<(), SyncError> {
	if let Some(key) = key {
		oneclient_sync_garbage::ActiveModel {
			object_key: Set(key),
			..Default::default()
		}
		.insert(db)
		.await?;
	}
	Ok(())
}

async fn usage(
	txn: &DatabaseTransaction,
	player_id: i32,
) -> Result<(Vec<oneclient_sync_record::Model>, i64), SyncError> {
	let records = OneclientSyncRecord::find()
		.filter(oneclient_sync_record::Column::PlayerId.eq(player_id))
		.filter(oneclient_sync_record::Column::Deleted.eq(false))
		.all(txn)
		.await?;
	let uploads = OneclientSyncUpload::find()
		.filter(
			oneclient_sync_upload::Column::RecordId
				.is_in(records.iter().map(|record| record.id)),
		)
		.filter(oneclient_sync_upload::Column::ExpiresAt.gt(Utc::now().fixed_offset()))
		.all(txn)
		.await?;

	let committed: i64 = records
		.iter()
		.filter(|record| record.object_key.is_some())
		.map(|record| record.size)
		.sum();
	let pending: i64 = uploads.iter().map(|upload| upload.size).sum();
	Ok((records, committed + pending))
}

fn check_quota(used: i64, size: i64) -> Result<(), SyncError> {
	if used.saturating_add(size) > MAX_ACCOUNT_BYTES {
		return Err(SyncError::LimitReached(format!(
			"Synced instances can use at most {} MiB in total",
			MAX_ACCOUNT_BYTES / 1024 / 1024
		)));
	}
	Ok(())
}

async fn prune_tombstones(
	txn: &DatabaseTransaction,
	player_id: i32,
) -> Result<(), SyncError> {
	let stale: Vec<i32> = OneclientSyncRecord::find()
		.select_only()
		.column(oneclient_sync_record::Column::Id)
		.filter(oneclient_sync_record::Column::PlayerId.eq(player_id))
		.filter(oneclient_sync_record::Column::Deleted.eq(true))
		.order_by_desc(oneclient_sync_record::Column::UpdatedAt)
		.offset(MAX_TOMBSTONES)
		.into_tuple()
		.all(txn)
		.await?;
	if !stale.is_empty() {
		OneclientSyncRecord::delete_many()
			.filter(oneclient_sync_record::Column::Id.is_in(stale))
			.exec(txn)
			.await?;
	}
	Ok(())
}

fn new_upload(
	record_id: i32,
	revision: i64,
	player: &user::Model,
	declared: &Declared,
	provisioned: bool,
	device: String,
) -> oneclient_sync_upload::ActiveModel {
	let id = Uuid::now_v7();
	oneclient_sync_upload::ActiveModel {
		id: Set(id),
		record_id: Set(record_id),
		revision: Set(revision),
		staging_key: Set(SyncStorage::staging_key(player.minecraft_uuid, id)),
		format: Set(declared.format),
		name: Set(declared.name.clone()),
		mc_version: Set(declared.mc_version.clone()),
		mc_loader: Set(declared.mc_loader.clone()),
		kind: Set(declared.kind.clone()),
		size: Set(declared.size),
		sha512: Set(declared.sha512.clone()),
		provisioned: Set(provisioned),
		device: Set(device),
		expires_at: Set((Utc::now() + UPLOAD_TTL).fixed_offset()),
	}
}

async fn upload_target(
	storage: &SyncStorage,
	upload: &oneclient_sync_upload::Model,
) -> Result<UploadTarget, SyncError> {
	Ok(UploadTarget {
		id: upload.id,
		url: storage.upload_url(&upload.staging_key, upload.size).await?,
		method: "PUT",
		headers: [("Content-Length", upload.size.to_string())].into(),
		expires_at: upload.expires_at.with_timezone(&Utc),
	})
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, SyncError> {
	storage(&state)?;
	if !matches!(query.mine.as_deref(), Some("1" | "true"))
		|| query.visibility.as_deref() != Some(PRIVATE)
	{
		return Err(SyncError::InvalidRequest(
			"Only mine=1&visibility=private can be listed".into(),
		));
	}
	let after = match query.cursor.as_deref() {
		Some(cursor) => cursor
			.parse::<i32>()
			.map_err(|_| SyncError::InvalidRequest("Invalid cursor".into()))?,
		None => 0,
	};

	let mut select = OneclientSyncRecord::find()
		.filter(oneclient_sync_record::Column::PlayerId.eq(player.id))
		.filter(oneclient_sync_record::Column::Revision.gt(0))
		.filter(oneclient_sync_record::Column::Id.gt(after));
	if let Some(since) = query.since {
		select = select
			.filter(oneclient_sync_record::Column::UpdatedAt.gt(since.fixed_offset()));
	}
	let mut records = select
		.order_by_asc(oneclient_sync_record::Column::Id)
		.limit(LIST_PAGE_SIZE + 1)
		.all(&state.database)
		.await?;

	let cursor = if records.len() as u64 > LIST_PAGE_SIZE {
		records.truncate(LIST_PAGE_SIZE as usize);
		records.last().map(|record| record.id.to_string())
	} else {
		None
	};

	Ok(Json(ListResponse {
		records: records.iter().map(record_view).collect(),
		cursor,
	}))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn get(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<RecordPath>,
) -> Result<Tagged<RecordResponse>, SyncError> {
	let storage = storage(&state)?;
	let code = parse_code(&path.id)?;
	let record = find_visible(&state.database, player.id, &code, false).await?;

	let download_url = match (&record.object_key, record.deleted) {
		(Some(key), false) => Some(storage.download_url(key).await?),
		_ => None,
	};

	Ok(Tagged(
		record.revision,
		RecordResponse {
			record: record_view(&record),
			download_url,
		},
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn payload(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<RecordPath>,
) -> Result<Response, SyncError> {
	let storage = storage(&state)?;
	let code = parse_code(&path.id)?;
	let record = find_visible(&state.database, player.id, &code, false).await?;
	if record.deleted {
		return Err(SyncError::Gone);
	}
	let key = record.object_key.as_deref().ok_or(SyncError::NotFound)?;
	let url = storage.download_url(key).await?;

	let mut response = StatusCode::FOUND.into_response();
	let headers = response.headers_mut();
	headers.insert(
		header::LOCATION,
		HeaderValue::from_str(&url)
			.map_err(|error| SyncError::Io(std::io::Error::other(error)))?,
	);
	if let Ok(value) = HeaderValue::from_str(&etag(record.revision)) {
		headers.insert(header::ETAG, value);
	}
	Ok(response)
}

#[tracing::instrument(level = "debug", skip(state, body))]
async fn create(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<UploadRequest>,
) -> Result<(StatusCode, Tagged<ReservedResponse>), SyncError> {
	let storage = storage(&state)?;
	if body.visibility.as_deref() != Some(PRIVATE) {
		return Err(SyncError::InvalidRequest(
			"Only private shares are supported".into(),
		));
	}
	let (declared, provisioned, device) = validate(&body)?;
	storage.check_write(player.id, declared.size).await?;

	let txn = state.database.begin().await?;
	lock_player(&txn, &player).await?;

	let (records, used) = usage(&txn, player.id).await?;
	if records.len() as u64 >= MAX_LIVE_RECORDS {
		return Err(SyncError::LimitReached(format!(
			"You can sync at most {MAX_LIVE_RECORDS} instances"
		)));
	}
	check_quota(used, declared.size)?;
	prune_tombstones(&txn, player.id).await?;

	let mut code = None;
	for _ in 0..CODE_ATTEMPTS {
		let candidate = generate_code();
		if find_record(&txn, player.id, &candidate, false)
			.await?
			.is_none()
		{
			code = Some(candidate);
			break;
		}
	}
	let code = code.ok_or(SyncError::Unavailable)?;

	let record = oneclient_sync_record::ActiveModel {
		player_id: Set(player.id),
		code: Set(code),
		revision: Set(0),
		format: Set(declared.format),
		name: Set(declared.name.clone()),
		mc_version: Set(declared.mc_version.clone()),
		mc_loader: Set(declared.mc_loader.clone()),
		kind: Set(declared.kind.clone()),
		size: Set(declared.size),
		sha512: Set(declared.sha512.clone()),
		provisioned: Set(provisioned),
		updated_by: Set(device.clone()),
		object_key: Set(None),
		deleted: Set(false),
		..Default::default()
	}
	.insert(&txn)
	.await?;
	let upload = new_upload(record.id, 1, &player, &declared, provisioned, device)
		.insert(&txn)
		.await?;
	txn.commit().await?;

	let target = upload_target(storage, &upload).await?;
	Ok((
		StatusCode::CREATED,
		Tagged(
			upload.revision,
			ReservedResponse {
				record: reserved_view(&record, &upload),
				upload: target,
			},
		),
	))
}

#[tracing::instrument(level = "debug", skip(state, headers, body))]
async fn update(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<RecordPath>,
	headers: HeaderMap,
	Json(body): Json<UploadRequest>,
) -> Result<Tagged<ReservedResponse>, SyncError> {
	let storage = storage(&state)?;
	let code = parse_code(&path.id)?;
	let if_match = parse_if_match(&headers)?.ok_or(SyncError::PreconditionRequired)?;
	let (declared, provisioned, device) = validate(&body)?;
	storage.check_write(player.id, declared.size).await?;

	let txn = state.database.begin().await?;
	lock_player(&txn, &player).await?;

	let record = find_visible(&txn, player.id, &code, true).await?;
	if record.deleted {
		return Err(SyncError::Gone);
	}
	if record.revision != if_match {
		return Err(conflict(&record));
	}

	if let Some(open) = OneclientSyncUpload::find()
		.filter(oneclient_sync_upload::Column::RecordId.eq(record.id))
		.one(&txn)
		.await?
	{
		if open.expires_at.with_timezone(&Utc) > Utc::now() {
			return Err(conflict(&record));
		}
		discard(&txn, Some(open.staging_key.clone())).await?;
		open.into_active_model().delete(&txn).await?;
	}

	let (_, used) = usage(&txn, player.id).await?;
	let current = if record.object_key.is_some() {
		record.size
	} else {
		0
	};
	check_quota(used - current, declared.size)?;

	let upload = new_upload(
		record.id,
		record.revision + 1,
		&player,
		&declared,
		provisioned,
		device,
	)
	.insert(&txn)
	.await?;
	txn.commit().await?;

	let target = upload_target(storage, &upload).await?;
	Ok(Tagged(
		upload.revision,
		ReservedResponse {
			record: reserved_view(&record, &upload),
			upload: target,
		},
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn commit(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<RecordPath>,
	Json(body): Json<CommitRequest>,
) -> Result<Tagged<CommittedResponse>, SyncError> {
	let storage = storage(&state)?;
	let code = parse_code(&path.id)?;
	let record = find_record(&state.database, player.id, &code, false)
		.await?
		.ok_or(SyncError::NotFound)?;
	if record.deleted {
		return Err(SyncError::Gone);
	}

	let Some(upload) = OneclientSyncUpload::find_by_id(body.upload_id)
		.filter(oneclient_sync_upload::Column::RecordId.eq(record.id))
		.one(&state.database)
		.await?
	else {
		if record
			.object_key
			.as_deref()
			.is_some_and(|key| SyncStorage::is_payload_of(key, body.upload_id))
		{
			return Ok(Tagged(
				record.revision,
				CommittedResponse {
					record: record_view(&record),
					removed: Vec::new(),
				},
			));
		}
		return Err(if record.revision > 0 {
			conflict(&record)
		} else {
			SyncError::NotFound
		});
	};

	storage.check_write(player.id, upload.size).await?;
	let _slot = storage.validation_slot(player.id).await?;

	match storage.uploaded_size(&upload.staging_key).await? {
		None => return Err(SyncError::UploadMissing),
		Some(size) if size == upload.size => {}
		Some(_) => {
			reject_upload(&state, &upload).await?;
			return Err(SyncError::InvalidPayload(
				"The uploaded payload does not match the declared size".into(),
			));
		}
	}

	let input = TempFile::new();
	let output = TempFile::new();
	storage.download(&upload.staging_key, input.path()).await?;

	let declared = Declared {
		format: upload.format,
		name: upload.name.clone(),
		mc_version: upload.mc_version.clone(),
		mc_loader: upload.mc_loader.clone(),
		kind: upload.kind.clone(),
		size: upload.size,
		sha512: upload.sha512.clone(),
	};
	let (input_path, output_path) =
		(input.path().to_path_buf(), output.path().to_path_buf());
	let sanitized = tokio::task::spawn_blocking(move || {
		payload::sanitize(&input_path, &output_path, &declared)
	})
	.await
	.map_err(|error| SyncError::Io(std::io::Error::other(error)))?;

	let sanitized = match sanitized {
		Ok(sanitized) => sanitized,
		Err(PayloadError::Io(error)) => return Err(error.into()),
		Err(error) => {
			reject_upload(&state, &upload).await?;
			return Err(match error {
				PayloadError::Secret { .. } => {
					SyncError::SecretDetected(error.to_string())
				}
				other => SyncError::InvalidPayload(other.to_string()),
			});
		}
	};

	let key = SyncStorage::payload_key(player.minecraft_uuid, &record.code, upload.id);
	let source = if sanitized.rewritten {
		output.path()
	} else {
		input.path()
	};
	storage.store(source, &key).await?;

	let txn = state.database.begin().await?;
	let Some(record) = find_record(&txn, player.id, &code, true).await? else {
		discard(&txn, Some(key)).await?;
		txn.commit().await?;
		return Err(SyncError::NotFound);
	};
	let still_reserved = OneclientSyncUpload::find_by_id(upload.id)
		.one(&txn)
		.await?
		.is_some();
	if record.deleted || !still_reserved {
		discard(&txn, Some(key)).await?;
		txn.commit().await?;
		return Err(if record.deleted {
			SyncError::Gone
		} else {
			conflict(&record)
		});
	}

	discard(&txn, record.object_key.clone()).await?;
	discard(&txn, Some(upload.staging_key.clone())).await?;
	OneclientSyncUpload::delete_by_id(upload.id)
		.exec(&txn)
		.await?;

	let mut active = record.into_active_model();
	active.revision = Set(upload.revision);
	active.format = Set(upload.format);
	active.name = Set(upload.name.clone());
	active.mc_version = Set(upload.mc_version.clone());
	active.mc_loader = Set(upload.mc_loader.clone());
	active.kind = Set(upload.kind.clone());
	active.size = Set(sanitized.size);
	active.sha512 = Set(sanitized.sha512.clone());
	active.provisioned = Set(upload.provisioned);
	active.updated_by = Set(upload.device.clone());
	active.object_key = Set(Some(key));
	active.updated_at = Set(Utc::now().fixed_offset());
	let record = active.update(&txn).await?;
	txn.commit().await?;

	if !sanitized.removed.is_empty() {
		tracing::info!(
			player = player.id,
			record = %record.code,
			removed = sanitized.removed.len(),
			"removed files from a synced instance"
		);
	}

	Ok(Tagged(
		record.revision,
		CommittedResponse {
			record: record_view(&record),
			removed: sanitized.removed,
		},
	))
}

async fn reject_upload(
	state: &ApiState,
	upload: &oneclient_sync_upload::Model,
) -> Result<(), SyncError> {
	let txn = state.database.begin().await?;
	let Some(record) = OneclientSyncRecord::find_by_id(upload.record_id)
		.lock_exclusive()
		.one(&txn)
		.await?
	else {
		return Ok(());
	};
	if OneclientSyncUpload::delete_by_id(upload.id)
		.exec(&txn)
		.await?
		.rows_affected
		> 0
	{
		discard(&txn, Some(upload.staging_key.clone())).await?;
		if record.revision == 0 {
			record.into_active_model().delete(&txn).await?;
		}
	}
	txn.commit().await?;
	Ok(())
}

#[tracing::instrument(level = "debug", skip(state, headers))]
async fn delete(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<RecordPath>,
	headers: HeaderMap,
) -> Result<StatusCode, SyncError> {
	storage(&state)?;
	let code = parse_code(&path.id)?;
	let if_match = parse_if_match(&headers)?;

	let txn = state.database.begin().await?;
	let record = find_record(&txn, player.id, &code, true)
		.await?
		.ok_or(SyncError::NotFound)?;
	if record.deleted {
		return Ok(StatusCode::NO_CONTENT);
	}
	if record.revision > 0
		&& let Some(expected) = if_match
		&& expected != record.revision
	{
		return Err(conflict(&record));
	}

	if let Some(open) = OneclientSyncUpload::find()
		.filter(oneclient_sync_upload::Column::RecordId.eq(record.id))
		.one(&txn)
		.await?
	{
		discard(&txn, Some(open.staging_key.clone())).await?;
		open.into_active_model().delete(&txn).await?;
	}
	discard(&txn, record.object_key.clone()).await?;

	if record.revision == 0 {
		record.into_active_model().delete(&txn).await?;
	} else {
		let revision = record.revision + 1;
		let mut active = record.into_active_model();
		active.deleted = Set(true);
		active.revision = Set(revision);
		active.object_key = Set(None);
		active.updated_at = Set(Utc::now().fixed_offset());
		active.update(&txn).await?;
	}
	txn.commit().await?;

	Ok(StatusCode::NO_CONTENT)
}
