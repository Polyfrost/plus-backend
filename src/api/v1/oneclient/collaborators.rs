use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, get_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
};
use chrono::{DateTime, Utc};
use entities::{oneclient_share_collaborator, prelude::*, user};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
	Set,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
	MAX_COLLABORATORS, SyncError,
	records::{PUBLIC, RecordPath, find_code, is_collaborator, parse_code, storage},
};
use crate::api::{ApiState, v0::account::AuthenticatedPlayer};

const MAX_USERNAME_CHARS: usize = 16;

#[derive(Debug, Serialize, JsonSchema)]
struct CollaboratorView {
	uuid: Uuid,
	username: Option<String>,
	added_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CollaboratorList {
	collaborators: Vec<CollaboratorView>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddRequest {
	username: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CollaboratorPath {
	id: String,
	uuid: Uuid,
}

#[derive(Debug, Deserialize)]
struct MojangProfile {
	id: String,
	name: String,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listShareCollaborators")
		.summary("List who can update a shared instance")
		.description("Only the owner of the share can list its collaborators.")
		.tag("oneclient")
}

fn add_doc(op: TransformOperation) -> TransformOperation {
	op.id("addShareCollaborator")
		.summary("Let another player update a shared instance")
		.description(
			"Adds a collaborator by Minecraft username. Collaborators can upload new \
			 revisions of the share, always with `If-Match`, but can't delete it or \
			 change who collaborates. The player must have signed in to OneClient \
			 before. A share can have up to 10 collaborators.",
		)
		.tag("oneclient")
}

fn remove_doc(op: TransformOperation) -> TransformOperation {
	op.id("removeShareCollaborator")
		.summary("Stop a player from updating a shared instance")
		.description("The owner can remove anyone; a collaborator can remove themselves.")
		.tag("oneclient")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The player can no longer update the share")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/shares/{id}/collaborators",
			get_with(self::list, self::list_doc).post_with(self::add, self::add_doc),
		)
		.api_route(
			"/shares/{id}/collaborators/{uuid}",
			delete_with(self::remove, self::remove_doc),
		)
}

async fn owned_share(
	state: &ApiState,
	player_id: i32,
	raw: &str,
) -> Result<entities::oneclient_sync_record::Model, SyncError> {
	storage(state)?;
	let code = parse_code(raw)?;
	let record = find_code(&state.database, &code)
		.await?
		.filter(|record| {
			record.revision > 0
				&& record.player_id == player_id
				&& record.visibility == PUBLIC
		})
		.ok_or(SyncError::NotFound)?;
	if record.deleted {
		return Err(SyncError::Gone);
	}
	Ok(record)
}

async fn view(
	state: &ApiState,
	row: &oneclient_share_collaborator::Model,
) -> Result<Option<CollaboratorView>, SyncError> {
	Ok(User::find_by_id(row.player_id)
		.one(&state.database)
		.await?
		.map(|user| CollaboratorView {
			uuid: user.minecraft_uuid,
			username: user.username,
			added_at: row.added_at.with_timezone(&Utc),
		}))
}

async fn collaborators(
	state: &ApiState,
	record_id: i32,
) -> Result<Vec<CollaboratorView>, SyncError> {
	let rows = OneclientShareCollaborator::find()
		.filter(oneclient_share_collaborator::Column::RecordId.eq(record_id))
		.order_by_asc(oneclient_share_collaborator::Column::AddedAt)
		.all(&state.database)
		.await?;
	let mut views = Vec::with_capacity(rows.len());
	for row in &rows {
		views.extend(view(state, row).await?);
	}
	Ok(views)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<RecordPath>,
) -> Result<Json<CollaboratorList>, SyncError> {
	let record = owned_share(&state, player.id, &path.id).await?;
	Ok(Json(CollaboratorList {
		collaborators: collaborators(&state, record.id).await?,
	}))
}

async fn resolve_player(
	state: &ApiState,
	username: &str,
) -> Result<user::Model, SyncError> {
	let username = username.trim();
	if username.is_empty()
		|| username.len() > MAX_USERNAME_CHARS
		|| !username
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || c == '_')
	{
		return Err(SyncError::InvalidRequest(
			"That isn't a Minecraft username".into(),
		));
	}

	let response = state
		.client
		.get(format!(
			"https://api.mojang.com/users/profiles/minecraft/{username}"
		))
		.send()
		.await
		.map_err(|error| {
			tracing::warn!(%error, "could not look up a Minecraft username");
			SyncError::Unavailable
		})?;
	let not_found =
		|| SyncError::InvalidRequest(format!("No Minecraft player is called {username}"));
	if matches!(
		response.status(),
		reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::NO_CONTENT
	) {
		return Err(not_found());
	}
	let profile: MojangProfile = response
		.error_for_status()
		.map_err(|error| {
			tracing::warn!(%error, "the Minecraft username lookup failed");
			SyncError::Unavailable
		})?
		.json()
		.await
		.map_err(|_| not_found())?;
	let uuid = Uuid::parse_str(&profile.id).map_err(|_| not_found())?;

	User::find()
		.filter(user::Column::MinecraftUuid.eq(uuid))
		.one(&state.database)
		.await?
		.filter(|user| !user.blacklisted)
		.ok_or_else(|| {
			SyncError::InvalidRequest(format!(
				"{} hasn't signed in to OneClient yet",
				profile.name
			))
		})
}

#[tracing::instrument(level = "debug", skip(state))]
async fn add(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<RecordPath>,
	Json(body): Json<AddRequest>,
) -> Result<(StatusCode, Json<CollaboratorView>), SyncError> {
	let record = owned_share(&state, player.id, &path.id).await?;
	storage(&state)?.check_write(player.id, 0).await?;
	let added = resolve_player(&state, &body.username).await?;
	if added.id == player.id {
		return Err(SyncError::InvalidRequest(
			"You already own this share".into(),
		));
	}

	if !is_collaborator(&state.database, record.id, added.id).await? {
		let count = OneclientShareCollaborator::find()
			.filter(oneclient_share_collaborator::Column::RecordId.eq(record.id))
			.count(&state.database)
			.await?;
		if count >= MAX_COLLABORATORS {
			return Err(SyncError::LimitReached(format!(
				"A share can have at most {MAX_COLLABORATORS} collaborators"
			)));
		}
		oneclient_share_collaborator::ActiveModel {
			record_id: Set(record.id),
			player_id: Set(added.id),
			..Default::default()
		}
		.insert(&state.database)
		.await?;
	}

	let row = OneclientShareCollaborator::find()
		.filter(oneclient_share_collaborator::Column::RecordId.eq(record.id))
		.filter(oneclient_share_collaborator::Column::PlayerId.eq(added.id))
		.one(&state.database)
		.await?
		.ok_or(SyncError::NotFound)?;
	let view = view(&state, &row).await?.ok_or(SyncError::NotFound)?;
	Ok((StatusCode::CREATED, Json(view)))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn remove(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(path): Path<CollaboratorPath>,
) -> Result<StatusCode, SyncError> {
	storage(&state)?;
	let code = parse_code(&path.id)?;
	let record = find_code(&state.database, &code)
		.await?
		.filter(|record| record.revision > 0)
		.ok_or(SyncError::NotFound)?;
	let leaving = player.minecraft_uuid == path.uuid;
	if record.player_id != player.id && !leaving {
		return Err(SyncError::NotFound);
	}
	let Some(removed) = User::find()
		.filter(user::Column::MinecraftUuid.eq(path.uuid))
		.one(&state.database)
		.await?
	else {
		return Ok(StatusCode::NO_CONTENT);
	};
	OneclientShareCollaborator::delete_many()
		.filter(oneclient_share_collaborator::Column::RecordId.eq(record.id))
		.filter(oneclient_share_collaborator::Column::PlayerId.eq(removed.id))
		.exec(&state.database)
		.await?;
	Ok(StatusCode::NO_CONTENT)
}
