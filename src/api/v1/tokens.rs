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
	ApiState, admin_auth::AdminAuthenticationExtractor, api_tokens::generate_token,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum TokenError {
	#[error("A label is required")]
	InvalidLabel,
	#[error("At least one exempt prefix is required, each an absolute path")]
	InvalidPrefixes,
	#[error("No such token")]
	NotFound,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for TokenError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::InvalidLabel | Self::InvalidPrefixes => StatusCode::BAD_REQUEST,
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
	exempt_prefixes: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TokenInfo {
	id: i32,
	label: String,
	exempt_prefixes: Vec<String>,
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
			created_at: value.created_at,
			revoked_at: value.revoked_at,
		}
	}
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createApiToken")
		.summary("Create an API token")
		.description(
			"Issues a token that lifts the per-address rate limit on the given \
			 path prefixes, for handing to a first-party service whose traffic \
			 should not be throttled. The secret is returned once, in this \
			 response, and only its hash is stored - a lost token has to be \
			 revoked and replaced. Admin password required.",
		)
		.tag("tokens")
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listApiTokens")
		.summary("List API tokens")
		.description(
			"Lists every token, revoked ones included, without their secrets. \
			 Admin password required.",
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
			 Admin password required.",
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
/// tricked into matching by a path that merely contains it.
fn validate_prefixes(prefixes: &[String]) -> bool {
	!prefixes.is_empty() && prefixes.iter().all(|prefix| prefix.starts_with('/'))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<CreateRequest>,
) -> Result<(StatusCode, Json<CreatedToken>), TokenError> {
	use entities::{api_token, prelude::*};

	let label = body.label.trim();
	if label.is_empty() {
		return Err(TokenError::InvalidLabel);
	}
	if !validate_prefixes(&body.exempt_prefixes) {
		return Err(TokenError::InvalidPrefixes);
	}

	let generated = generate_token();

	let created = ApiToken::insert(api_token::ActiveModel {
		label: Set(label.to_owned()),
		token_hash: Set(generated.hash),
		exempt_prefixes: Set(body.exempt_prefixes),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	tracing::info!(id = created.id, label = %created.label, "Issued an api token");

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
	_auth: AdminAuthenticationExtractor,
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
	_auth: AdminAuthenticationExtractor,
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
