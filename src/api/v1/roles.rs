use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, post_with, put_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::IntoResponse,
};
use entities::{prelude::*, role, user, user_role};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, SqlErr,
	sea_query::OnConflict,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		permissions::{self, Require, RequirePlayer, perm},
		v0::account::AuthenticatedPlayer,
	},
	database::DatabaseUserExt,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum RoleError {
	#[error("A role name is required, at most 64 characters")]
	InvalidName,
	#[error("Colours are 0xRRGGBB, between 0 and 16777215")]
	InvalidColor,
	#[error("Permissions must be an unsigned 64-bit integer, as a string")]
	InvalidPermissions,
	#[error("A role with that name already exists")]
	NameTaken,
	#[error("No such role")]
	NotFound,
	#[error(
		"You can only manage roles and players below your highest role, with \
		 permissions you have"
	)]
	Hierarchy,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for RoleError {
	fn into_response(self) -> axum::response::Response {
		// Not `admin_error_response`: `/v1/me` answers any player.
		crate::api::error_response(
			match self {
				Self::InvalidName | Self::InvalidColor | Self::InvalidPermissions => {
					StatusCode::BAD_REQUEST
				}
				Self::NameTaken => StatusCode::CONFLICT,
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::Hierarchy => StatusCode::FORBIDDEN,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RoleInfo {
	id: i32,
	name: String,
	/// 0xRRGGBB, or null for no colour.
	color: Option<i32>,
	/// Higher outranks lower.
	position: i32,
	/// u64 permission flags as a decimal string, since JSON numbers lose
	/// precision above 2^53.
	permissions: String,
}

impl From<role::Model> for RoleInfo {
	fn from(value: role::Model) -> Self {
		RoleInfo {
			permissions: permissions::flags(&value).to_string(),
			id: value.id,
			name: value.name,
			color: value.color,
			position: value.position,
		}
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RoleRequest {
	name: String,
	color: Option<i32>,
	position: i32,
	/// u64 permission flags as a decimal string.
	permissions: String,
}

impl RoleRequest {
	/// Trims the name in place and returns the parsed permission flags.
	fn validate(&mut self) -> Result<u64, RoleError> {
		self.name = self.name.trim().to_owned();
		if self.name.is_empty() || self.name.chars().count() > 64 {
			return Err(RoleError::InvalidName);
		}
		if self.color.is_some_and(|color| !(0..=0xFF_FFFF).contains(&color)) {
			return Err(RoleError::InvalidColor);
		}
		self.permissions
			.parse::<u64>()
			.map_err(|_| RoleError::InvalidPermissions)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Member {
	id: Uuid,
	username: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Me {
	id: Uuid,
	username: Option<String>,
	/// The union of every role's flags, as a decimal string.
	permissions: String,
	/// Highest position first.
	roles: Vec<RoleInfo>,
}

fn doc(
	id: &'static str,
	summary: &'static str,
) -> impl FnOnce(TransformOperation) -> TransformOperation {
	move |op| op.id(id).summary(summary).tag("roles")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/me", get_with(self::me, doc("getMe", "The caller's roles and permissions")))
		.api_route("/roles", get_with(self::list, doc("listRoles", "List roles")))
		.api_route("/roles", post_with(self::create, doc("createRole", "Create a role")))
		.api_route("/roles/{id}", put_with(self::update, doc("updateRole", "Replace a role")))
		.api_route("/roles/{id}", delete_with(self::delete, doc("deleteRole", "Delete a role")))
		.api_route(
			"/roles/{id}/members",
			get_with(self::members, doc("listRoleMembers", "List a role's members")),
		)
		.api_route(
			"/players/{player}/roles",
			get_with(self::player_roles, doc("listPlayerRoles", "List a player's roles")),
		)
		.api_route(
			"/players/{player}/roles/{id}",
			put_with(self::assign, doc("assignRole", "Give a player a role")),
		)
		.api_route(
			"/players/{player}/roles/{id}",
			delete_with(self::unassign, doc("unassignRole", "Take a role from a player")),
		)
}

/// Rejects changing `target`'s roles unless the caller outranks them. Players
/// may always change their own roles, within what `guard` allows.
async fn guard_member(
	state: &ApiState,
	actor: &user::Model,
	target: &user::Model,
) -> Result<(), RoleError> {
	if actor.id == target.id {
		return Ok(());
	}
	let actor_roles = permissions::roles_of(&state.database, actor.id).await?;
	let target_roles = permissions::roles_of(&state.database, target.id).await?;
	if permissions::may_manage_member(&actor_roles, &target_roles) {
		Ok(())
	} else {
		Err(RoleError::Hierarchy)
	}
}

/// Rejects the change unless the caller outranks the role and holds its flags.
async fn guard(
	state: &ApiState,
	actor: &user::Model,
	position: i32,
	flags: u64,
) -> Result<(), RoleError> {
	let roles = permissions::roles_of(&state.database, actor.id).await?;
	if permissions::may_manage(&roles, position, flags) {
		Ok(())
	} else {
		Err(RoleError::Hierarchy)
	}
}

async fn find_role(state: &ApiState, id: i32) -> Result<role::Model, RoleError> {
	Role::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(RoleError::NotFound)
}

fn name_taken(error: sea_orm::DbErr) -> RoleError {
	match error.sql_err() {
		Some(SqlErr::UniqueConstraintViolation(_)) => RoleError::NameTaken,
		_ => error.into(),
	}
}

#[tracing::instrument(level = "debug", skip(state))]
async fn me(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Me>, RoleError> {
	let roles = permissions::roles_of(&state.database, player.id).await?;

	Ok(Json(Me {
		id: player.minecraft_uuid,
		username: player.username,
		permissions: permissions::union(&roles).to_string(),
		roles: roles.into_iter().map(RoleInfo::from).collect(),
	}))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn list(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_ROLES }>,
) -> Result<Json<Vec<RoleInfo>>, RoleError> {
	Ok(Json(
		Role::find()
			.order_by_desc(role::Column::Position)
			.order_by_asc(role::Column::Id)
			.all(&state.database)
			.await?
			.into_iter()
			.map(RoleInfo::from)
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn create(
	State(state): State<ApiState>,
	RequirePlayer(actor): RequirePlayer<{ perm::MANAGE_ROLES }>,
	Json(mut body): Json<RoleRequest>,
) -> Result<(StatusCode, Json<RoleInfo>), RoleError> {
	let flags = body.validate()?;
	guard(&state, &actor, body.position, flags).await?;

	let created = Role::insert(role::ActiveModel {
		name: Set(body.name),
		color: Set(body.color),
		position: Set(body.position),
		permissions: Set(flags as i64),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await
	.map_err(name_taken)?;

	tracing::info!(id = created.id, name = %created.name, by = %actor.minecraft_uuid, "Created a role");

	Ok((StatusCode::CREATED, Json(created.into())))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn update(
	State(state): State<ApiState>,
	RequirePlayer(actor): RequirePlayer<{ perm::MANAGE_ROLES }>,
	Path(id): Path<i32>,
	Json(mut body): Json<RoleRequest>,
) -> Result<Json<RoleInfo>, RoleError> {
	let flags = body.validate()?;
	let existing = find_role(&state, id).await?;
	// Both what it is now and what it would become must be below the caller.
	guard(&state, &actor, existing.position, permissions::flags(&existing)).await?;
	guard(&state, &actor, body.position, flags).await?;

	let mut active: role::ActiveModel = existing.into();
	active.name = Set(body.name);
	active.color = Set(body.color);
	active.position = Set(body.position);
	active.permissions = Set(flags as i64);
	let updated = active.update(&state.database).await.map_err(name_taken)?;

	tracing::info!(id, name = %updated.name, by = %actor.minecraft_uuid, "Updated a role");

	Ok(Json(updated.into()))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn delete(
	State(state): State<ApiState>,
	RequirePlayer(actor): RequirePlayer<{ perm::MANAGE_ROLES }>,
	Path(id): Path<i32>,
) -> Result<StatusCode, RoleError> {
	let existing = find_role(&state, id).await?;
	guard(&state, &actor, existing.position, permissions::flags(&existing)).await?;

	Role::delete_by_id(id).exec(&state.database).await?;
	tracing::info!(id, name = %existing.name, by = %actor.minecraft_uuid, "Deleted a role");

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn members(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_ROLES }>,
	Path(id): Path<i32>,
) -> Result<Json<Vec<Member>>, RoleError> {
	Ok(Json(
		User::find()
			.inner_join(UserRole)
			.filter(user_role::Column::RoleId.eq(id))
			.order_by_asc(user::Column::Username)
			.all(&state.database)
			.await?
			.into_iter()
			.map(|player| Member {
				id: player.minecraft_uuid,
				username: player.username,
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn player_roles(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_ROLES }>,
	Path(player): Path<Uuid>,
) -> Result<Json<Vec<RoleInfo>>, RoleError> {
	let Some(player) = User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.one(&state.database)
		.await?
	else {
		return Ok(Json(Vec::new()));
	};

	Ok(Json(
		permissions::roles_of(&state.database, player.id)
			.await?
			.into_iter()
			.map(RoleInfo::from)
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn assign(
	State(state): State<ApiState>,
	RequirePlayer(actor): RequirePlayer<{ perm::MANAGE_ROLES }>,
	Path((player, id)): Path<(Uuid, i32)>,
) -> Result<StatusCode, RoleError> {
	let role = find_role(&state, id).await?;
	guard(&state, &actor, role.position, permissions::flags(&role)).await?;

	// Players who never logged in can still be given a role ahead of time.
	let player = User::get_or_create(&state.database, player).await?;
	guard_member(&state, &actor, &player).await?;
	UserRole::insert(user_role::ActiveModel {
		user_id: Set(player.id),
		role_id: Set(id),
	})
	.on_conflict(
		OnConflict::columns([user_role::Column::UserId, user_role::Column::RoleId])
			.do_nothing()
			.to_owned(),
	)
	.exec_without_returning(&state.database)
	.await?;

	tracing::info!(role = %role.name, player = %player.minecraft_uuid, by = %actor.minecraft_uuid, "Assigned a role");

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn unassign(
	State(state): State<ApiState>,
	RequirePlayer(actor): RequirePlayer<{ perm::MANAGE_ROLES }>,
	Path((player, id)): Path<(Uuid, i32)>,
) -> Result<StatusCode, RoleError> {
	let role = find_role(&state, id).await?;
	guard(&state, &actor, role.position, permissions::flags(&role)).await?;

	let Some(player) = User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.one(&state.database)
		.await?
	else {
		return Ok(StatusCode::NO_CONTENT);
	};
	guard_member(&state, &actor, &player).await?;

	UserRole::delete_by_id((player.id, id))
		.exec(&state.database)
		.await?;
	tracing::info!(role = %role.name, player = %player.minecraft_uuid, by = %actor.minecraft_uuid, "Unassigned a role");

	Ok(StatusCode::NO_CONTENT)
}
