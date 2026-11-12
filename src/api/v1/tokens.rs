use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset, Utc};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, EntityTrait, QueryOrder, Set};
use serde::{Deserialize, Serialize};

use crate::api::{
	ApiState,
	api_tokens::generate_token,
	permissions::{self, Require, RequirePlayer, perm},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum TokenError {
	#[error("A label is required")]
	InvalidLabel,
	#[error("Give the token an exempt prefix or a permission; prefixes are absolute paths")]
	InvalidPrefixes,
	#[error("Permissions must be an unsigned 64-bit integer, as a string")]
	InvalidPermissions,
	#[error("API tokens cannot hold Administrator, Manage roles or Manage API tokens")]
	ForbiddenPermissions,
	#[error("You can only give a token permissions you hold yourself")]
	MissingPermissions,
	#[error("No such token")]
	NotFound,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for TokenError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::InvalidLabel
				| Self::InvalidPrefixes
				| Self::InvalidPermissions
				| Self::ForbiddenPermissions => StatusCode::BAD_REQUEST,
				Self::MissingPermissions => StatusCode::FORBIDDEN,
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateRequest {
	/// What this token is for, so a stale one can be recognised later.
	label: String,
	/// Request path prefixes this token lifts the rate limit on. `/` exempts
	/// every endpoint.
	#[serde(default)]
	exempt_prefixes: Vec<String>,
	/// u64 permission flags as a decimal string, letting the service call
	/// endpoints guarded by them. Never ADMINISTRATOR, MANAGE_ROLES or
	/// MANAGE_API_TOKENS, and only flags the creator holds.
	#[serde(default)]
	permissions: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TokenInfo {
	id: i32,
	label: String,
	exempt_prefixes: Vec<String>,
	/// u64 permission flags as a decimal string.
	permissions: String,
	created_at: DateTime<FixedOffset>,
	revoked_at: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CreatedToken {
	#[serde(flatten)]
	info: TokenInfo,
	/// The secret to send in the `X-Api-Token` header. Only ever returned
	/// here: the API stores nothing but its hash.
	token: String,
}

impl From<entities::api_token::Model> for TokenInfo {
	fn from(value: entities::api_token::Model) -> Self {
		TokenInfo {
			id: value.id,
			label: value.label,
			exempt_prefixes: value.exempt_prefixes,
			permissions: (value.permissions as u64).to_string(),
			created_at: value.created_at,
			revoked_at: value.revoked_at,
		}
	}
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createApiToken")
		.summary("Create an API token")
		.description(
			"Issues a token for an external service, sent as `X-Api-Token`. It \
			 lifts the per-address rate limit on the given path prefixes, and \
			 with `permissions` lets the service call endpoints guarded by those \
			 flags (it never acts as a player). The secret is returned once, in this \
			 response, and only its hash is stored - a lost token has to be \
			 revoked and replaced. Requires `MANAGE_API_TOKENS`.",
		)
		.tag("tokens")
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listApiTokens")
		.summary("List API tokens")
		.description(
			"Lists every token, revoked ones included, without their secrets. \
			 Requires `MANAGE_API_TOKENS`.",
		)
		.tag("tokens")
}

fn revoke_doc(op: TransformOperation) -> TransformOperation {
	op.id("revokeApiToken")
		.summary("Revoke an API token")
		.description(
			"Stops a token exempting anything. The row is kept so the audit \
			 trail survives. Revoking is immediate on the replica that serves \
			 this request, and takes effect on the others within a minute. \
			 Requires `MANAGE_API_TOKENS`.",
		)
		.tag("tokens")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/tokens", post_with(self::create, self::create_doc))
		.api_route("/tokens", get_with(self::list, self::list_doc))
		.api_route("/tokens/{id}", delete_with(self::revoke, self::revoke_doc))
}

/// Prefixes have to be absolute, so that a token scoped to `/asset/` cannot be
/// tricked into matching by a path that merely contains it. A token with
/// neither prefixes nor permissions would do nothing.
fn validate_prefixes(prefixes: &[String], permissions: u64) -> bool {
	(!prefixes.is_empty() || permissions != 0)
		&& prefixes.iter().all(|prefix| prefix.starts_with('/'))
}

/// The flags a token may be given by someone holding `creator`.
fn validate_permissions(permissions: u64, creator: u64) -> Result<(), TokenError> {
	if permissions & permissions::KEY_FORBIDDEN != 0 {
		return Err(TokenError::ForbiddenPermissions);
	}
	if !permissions::has(creator, permissions) {
		return Err(TokenError::MissingPermissions);
	}
	Ok(())
}

#[tracing::instrument(level = "debug", skip(state))]
async fn create(
	State(state): State<ApiState>,
	RequirePlayer(creator): RequirePlayer<{ perm::MANAGE_API_TOKENS }>,
	Json(body): Json<CreateRequest>,
) -> Result<(StatusCode, Json<CreatedToken>), TokenError> {
	use entities::{api_token, prelude::*};

	let label = body.label.trim();
	if label.is_empty() {
		return Err(TokenError::InvalidLabel);
	}
	let token_permissions = body
		.permissions
		.as_deref()
		.map_or(Ok(0), str::parse::<u64>)
		.map_err(|_| TokenError::InvalidPermissions)?;
	if !validate_prefixes(&body.exempt_prefixes, token_permissions) {
		return Err(TokenError::InvalidPrefixes);
	}
	let creator_permissions =
		permissions::union(&permissions::roles_of(&state.database, creator.id).await?);
	validate_permissions(token_permissions, creator_permissions)?;

	let generated = generate_token();

	let created = ApiToken::insert(api_token::ActiveModel {
		label: Set(label.to_owned()),
		token_hash: Set(generated.hash),
		exempt_prefixes: Set(body.exempt_prefixes),
		permissions: Set(token_permissions as i64),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	tracing::info!(
		id = created.id,
		label = %created.label,
		permissions = token_permissions,
		by = %creator.minecraft_uuid,
		"Issued an api token"
	);

	Ok((
		StatusCode::CREATED,
		Json(CreatedToken {
			info: created.into(),
			token: generated.token,
		}),
	))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn list(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_API_TOKENS }>,
) -> Result<Json<Vec<TokenInfo>>, TokenError> {
	use entities::{api_token, prelude::*};

	Ok(Json(
		ApiToken::find()
			.order_by_asc(api_token::Column::Id)
			.all(&state.database)
			.await?
			.into_iter()
			.map(TokenInfo::from)
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn revoke(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_API_TOKENS }>,
	Path(id): Path<i32>,
) -> Result<Json<TokenInfo>, TokenError> {
	use entities::prelude::*;

	let token = ApiToken::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(TokenError::NotFound)?;
	if token.revoked_at.is_some() {
		return Ok(Json(token.into()));
	}

	let mut active: entities::api_token::ActiveModel = token.into();
	active.revoked_at = Set(Some(Utc::now().into()));
	let revoked = active.update(&state.database).await?;

	// Immediate on this replica; the others catch up within the cache ttl.
	state.api_tokens.resolved.invalidate(&revoked.token_hash).await;
	tracing::info!(id = revoked.id, label = %revoked.label, "Revoked an api token");

	Ok(Json(revoked.into()))
}

#[cfg(test)]
mod tests {
	use super::{TokenError, validate_permissions, validate_prefixes};
	use crate::api::permissions::perm;

	#[test]
	fn a_token_needs_a_prefix_or_a_permission() {
		assert!(validate_prefixes(&["/asset/".to_owned()], 0));
		assert!(validate_prefixes(&[], perm::VIEW_ANALYTICS));
		assert!(!validate_prefixes(&[], 0), "would do nothing");
		assert!(!validate_prefixes(&["asset/".to_owned()], perm::VIEW_ANALYTICS), "relative prefix");
	}

	#[test]
	fn tokens_only_get_safe_flags_their_creator_holds() {
		let creator = perm::MANAGE_API_TOKENS | perm::VIEW_ANALYTICS;

		assert!(validate_permissions(perm::VIEW_ANALYTICS, creator).is_ok());
		assert!(validate_permissions(0, creator).is_ok());
		assert!(matches!(
			validate_permissions(perm::MANAGE_STORE, creator),
			Err(TokenError::MissingPermissions)
		));
		for forbidden in [perm::ADMINISTRATOR, perm::MANAGE_ROLES, perm::MANAGE_API_TOKENS] {
			assert!(matches!(
				validate_permissions(forbidden, perm::ADMINISTRATOR),
				Err(TokenError::ForbiddenPermissions)
			));
		}
		// An administrator may hand out any other flag, including unnamed ones.
		assert!(validate_permissions(perm::MANAGE_STORE | 1 << 63, perm::ADMINISTRATOR).is_ok());
	}
}
