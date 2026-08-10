use aide::{
	OperationInput, OperationIo,
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{FromRequestParts, Path, State},
	http::{StatusCode, header, request::Parts},
	response::{IntoResponse, Redirect, Response},
};
use axum_client_ip::ClientIp;
use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set,
	TransactionTrait, TryInsertResult, sea_query::Expr,
};
use serde::{Deserialize, Serialize};

use crate::{
	api::{ApiState, admin_auth::AdminAuthenticationExtractor},
	utils::{
		hash::sha256_hex_parts,
		user_agent::is_bot,
		validation::{valid_http_url, valid_slug},
	},
};

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/go/{slug}", get_with(self::follow, self::redirect_doc))
		.api_route("/links", post_with(self::create, self::create_doc))
		.api_route("/links", get_with(self::list, self::list_doc))
		.api_route("/links/{slug}", delete_with(self::delete, self::delete_doc))
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum LinksError {
	#[error("No tracked link with that slug exists")]
	NotFound,
	#[error("A tracked link with that slug already exists")]
	SlugTaken,
	#[error("Slug must be non-empty and contain only a-z, 0-9, '-' or '_'")]
	InvalidSlug,
	#[error("Target url must be an absolute http(s) url")]
	InvalidUrl,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for LinksError {
	fn into_response(self) -> Response {
		crate::api::error_response(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::SlugTaken => StatusCode::CONFLICT,
				Self::InvalidSlug | Self::InvalidUrl => StatusCode::BAD_REQUEST,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

struct VisitorId {
	ip: std::net::IpAddr,
	user_agent: String,
}

impl OperationInput for VisitorId {
	fn operation_input(
		_ctx: &mut aide::generate::GenContext,
		_operation: &mut aide::openapi::Operation,
	) {
	}
}

impl FromRequestParts<ApiState> for VisitorId {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		let ClientIp(ip) = ClientIp::from_request_parts(parts, state)
			.await
			.map_err(IntoResponse::into_response)?;

		let user_agent = parts
			.headers
			.get(header::USER_AGENT)
			.and_then(|value| value.to_str().ok())
			.unwrap_or_default()
			.to_owned();

		Ok(Self { ip, user_agent })
	}
}

impl VisitorId {
	/// A stable, salted id for this visitor on one slug. The salt keeps the
	/// digest from being reversible into an ip, and including the slug stops
	/// visits to different links being correlated.
	fn hash(&self, salt: &str, slug: &str) -> String {
		sha256_hex_parts(&[
			salt.as_bytes(),
			self.ip.to_string().as_bytes(),
			self.user_agent.as_bytes(),
			slug.as_bytes(),
		])
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LinkInfo {
	slug: String,
	target_url: String,
	clicks: i64,
	created_at: DateTime<FixedOffset>,
}

impl LinkInfo {
	fn from_model(link: entities::tracked_links::Model) -> Self {
		Self {
			slug: link.slug,
			target_url: link.target_url,
			clicks: link.clicks,
			created_at: link.created_at,
		}
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateRequest {
	slug: String,
	target_url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListResponse {
	links: Vec<LinkInfo>,
}

fn redirect_doc(op: TransformOperation) -> TransformOperation {
	op.id("followTrackedLink")
		.summary("Follow a tracked link")
		.description(
			"Redirects to the link's target url, counting the visit if it is the \
			 visitor's first for this slug (deduplicated by a salted hash of ip + \
			 user-agent). Public. Returns 404 when no link with that slug exists.",
		)
		.tag("links")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createTrackedLink")
		.summary("Create a tracked link")
		.description(
			"Creates a tracked short link redirecting `/go/{slug}` to an arbitrary \
			 absolute http(s) url. Admin password required.",
		)
		.tag("links")
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listTrackedLinks")
		.summary("List tracked links")
		.description(
			"Lists every tracked link with its unique-visit count. Admin password \
			 required.",
		)
		.tag("links")
}

fn delete_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteTrackedLink")
		.summary("Delete a tracked link")
		.description(
			"Deletes the tracked link with the given slug. Admin password required.",
		)
		.tag("links")
}

#[tracing::instrument(level = "debug", skip(state, visitor))]
async fn follow(
	State(state): State<ApiState>,
	Path(slug): Path<String>,
	visitor: VisitorId,
) -> Result<Redirect, LinksError> {
	use entities::{prelude::*, tracked_link_hits, tracked_links};

	let link = TrackedLinks::find_by_id(&slug)
		.one(&state.database)
		.await?
		.ok_or(LinksError::NotFound)?;

	if !is_bot(&visitor.user_agent) {
		let visitor_hash = visitor.hash(&state.admin_password, &slug);
		let txn = state.database.begin().await?;

		let inserted = TrackedLinkHits::insert(tracked_link_hits::ActiveModel {
			slug: Set(slug.clone()),
			visitor_hash: Set(visitor_hash),
			..Default::default()
		})
		.on_conflict_do_nothing()
		.exec(&txn)
		.await?;

		if matches!(inserted, TryInsertResult::Inserted(_)) {
			TrackedLinks::update_many()
				.col_expr(
					tracked_links::Column::Clicks,
					Expr::col(tracked_links::Column::Clicks).add(1),
				)
				.filter(tracked_links::Column::Slug.eq(&slug))
				.exec(&txn)
				.await?;
		}

		txn.commit().await?;
	}

	Ok(Redirect::temporary(&link.target_url))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<CreateRequest>,
) -> Result<(StatusCode, Json<LinkInfo>), LinksError> {
	use entities::{prelude::*, tracked_links};

	if !valid_slug(&body.slug) {
		return Err(LinksError::InvalidSlug);
	}
	if !valid_http_url(&body.target_url) {
		return Err(LinksError::InvalidUrl);
	}

	if TrackedLinks::find_by_id(&body.slug)
		.one(&state.database)
		.await?
		.is_some()
	{
		return Err(LinksError::SlugTaken);
	}

	let link = tracked_links::ActiveModel {
		slug: Set(body.slug),
		target_url: Set(body.target_url),
		clicks: Set(0),
		..Default::default()
	}
	.insert(&state.database)
	.await?;

	Ok((StatusCode::CREATED, Json(LinkInfo::from_model(link))))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn list(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
) -> Result<Json<ListResponse>, LinksError> {
	use entities::{prelude::*, tracked_links};

	let links = TrackedLinks::find()
		.order_by_desc(tracked_links::Column::Clicks)
		.all(&state.database)
		.await?
		.into_iter()
		.map(LinkInfo::from_model)
		.collect();

	Ok(Json(ListResponse { links }))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn delete(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(slug): Path<String>,
) -> Result<StatusCode, LinksError> {
	use entities::prelude::*;

	let result = TrackedLinks::delete_by_id(slug)
		.exec(&state.database)
		.await?;

	if result.rows_affected == 0 {
		return Err(LinksError::NotFound);
	}

	Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
	use std::net::IpAddr;

	use super::VisitorId;

	fn visitor(ip: &str, user_agent: &str) -> VisitorId {
		VisitorId {
			ip: ip
				.parse::<IpAddr>()
				.expect("'ip' is not a valid ip address"),
			user_agent: user_agent.to_owned(),
		}
	}

	#[test]
	fn hash_is_stable_for_same_visitor_and_slug() {
		let v = visitor("203.0.113.7", "Chrome");
		assert_eq!(
			v.hash("salt", "oneclient"),
			v.hash("salt", "oneclient"),
			"same visitor + slug must hash identically so repeats dedupe",
		);
	}

	#[test]
	fn hash_differs_across_visitor_slug_and_salt() {
		let a = visitor("203.0.113.7", "Chrome");
		let b = visitor("203.0.113.8", "Chrome"); // different ip
		let c = visitor("203.0.113.7", "Firefox"); // different ua

		assert_ne!(a.hash("salt", "oneclient"), b.hash("salt", "oneclient"));
		assert_ne!(a.hash("salt", "oneclient"), c.hash("salt", "oneclient"));
		assert_ne!(a.hash("salt", "oneclient"), a.hash("salt", "oneconfig"));
		assert_ne!(a.hash("salt", "oneclient"), a.hash("other", "oneclient"));
	}

	#[test]
	fn hash_is_hex_sha256() {
		let h = visitor("203.0.113.7", "Chrome").hash("salt", "oneclient");
		assert_eq!(h.len(), 64);
		assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
	}
}
