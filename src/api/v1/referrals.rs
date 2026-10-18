use std::collections::HashMap;

use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::IntoResponse,
};
use entities::{prelude::*, referral, tracked_links, transaction};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, FromQueryResult,
	QueryFilter, QueryOrder, QuerySelect, Set, SqlErr,
	sea_query::{Alias, Expr, Func},
};
use serde::{Deserialize, Serialize};

use super::discounts::enabled_by_default;
use crate::{
	api::{ApiState, admin_auth::AdminAuthenticationExtractor},
	pricing::normalise_code,
	utils::validation::valid_slug,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ReferralError {
	#[error("No referral with that code exists")]
	NotFound,
	#[error("The code {0} is already in use")]
	DuplicateCode(String),
	#[error("Code must be non-empty and contain only A-Z, 0-9, '-' or '_'")]
	InvalidCode,
	#[error("Discount {0} does not exist or is a sale rather than a coupon")]
	NotACoupon(i32),
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for ReferralError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::DuplicateCode(_) => StatusCode::CONFLICT,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
				Self::InvalidCode | Self::NotACoupon(_) => StatusCode::BAD_REQUEST,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateBody {
	/// Stored and matched upper case.
	pub code: String,
	/// Who the code belongs to, e.g. the creator.
	pub name: String,
	/// A coupon applied automatically whenever the code is used.
	#[serde(default)]
	pub discount_id: Option<i32>,
	#[serde(default = "enabled_by_default")]
	pub enabled: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateBody {
	pub name: String,
	#[serde(default)]
	pub discount_id: Option<i32>,
	#[serde(default = "enabled_by_default")]
	pub enabled: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReferralView {
	pub code: String,
	pub name: String,
	pub discount_id: Option<i32>,
	pub enabled: bool,
	pub created_at: String,
	/// Orders placed with the code, refunded ones included.
	pub orders: i64,
	/// Charged minus refunded, chargebacks excluded, in the checkout currency.
	pub revenue_minor: i64,
	/// Unique visits across the tracked links carrying the code.
	pub clicks: i64,
}

#[derive(Debug, Default, FromQueryResult)]
struct Stats {
	referral_code: String,
	orders: i64,
	revenue_minor: Option<i64>,
}

#[derive(Debug, FromQueryResult)]
struct Clicks {
	referral_code: String,
	clicks: Option<i64>,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/admin/referrals",
			get_with(self::list, self::list_doc)
				.post_with(self::create, self::create_doc),
		)
		.api_route(
			"/admin/referrals/{code}",
			post_with(self::update, self::update_doc)
				.delete_with(self::remove, self::remove_doc),
		)
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listReferrals")
		.summary("List creator codes")
		.description(
			"Lists every creator code with the orders, revenue and tracked-link \
			 clicks attributed to it. Admin password required.",
		)
		.tag("referrals")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createReferral")
		.summary("Create a creator code")
		.description(
			"Creates a creator code buyers pass to checkout as `referral`. Link a \
			 coupon with `discount_id` and checkout applies it on its own; a \
			 coupon that has ended or run out is skipped rather than failing the \
			 checkout. Point a tracked link at the code and `/go/{slug}` adds \
			 `ref=CODE` to its target. Admin password required.",
		)
		.tag("referrals")
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateReferral")
		.summary("Replace a creator code")
		.description(
			"Replaces a creator code's name, coupon and enabled flag. The code \
			 itself cannot change. Admin password required.",
		)
		.tag("referrals")
}

fn remove_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteReferral")
		.summary("Delete a creator code")
		.description(
			"Deletes a creator code and unlinks its tracked links. Orders keep \
			 the code they were placed with; prefer disabling one in use. Admin \
			 password required.",
		)
		.tag("referrals")
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn list(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
) -> Result<Json<Vec<ReferralView>>, ReferralError> {
	let rows = Referral::find()
		.order_by_desc(referral::Column::CreatedAt)
		.all(&state.database)
		.await?;

	let mut stats: HashMap<String, Stats> = Transaction::find()
		.select_only()
		.column(transaction::Column::ReferralCode)
		.column_as(transaction::Column::Id.count(), "orders")
		.column_as(
			Expr::expr(Func::sum(
				Expr::case(
					transaction::Column::ChargedBackAt.is_null(),
					Expr::col(transaction::Column::AmountMinor)
						.sub(Expr::col(transaction::Column::RefundedMinor)),
				)
				.finally(0),
			))
			.cast_as(Alias::new("bigint")),
			"revenue_minor",
		)
		.filter(transaction::Column::ReferralCode.is_not_null())
		.group_by(transaction::Column::ReferralCode)
		.into_model::<Stats>()
		.all(&state.database)
		.await?
		.into_iter()
		.map(|mut row| (std::mem::take(&mut row.referral_code), row))
		.collect();

	let mut clicks: HashMap<String, i64> = TrackedLinks::find()
		.select_only()
		.column(tracked_links::Column::ReferralCode)
		.column_as(
			Expr::col(tracked_links::Column::Clicks)
				.sum()
				.cast_as(Alias::new("bigint")),
			"clicks",
		)
		.filter(tracked_links::Column::ReferralCode.is_not_null())
		.group_by(tracked_links::Column::ReferralCode)
		.into_model::<Clicks>()
		.all(&state.database)
		.await?
		.into_iter()
		.map(|row| (row.referral_code, row.clicks.unwrap_or_default()))
		.collect();

	Ok(Json(
		rows.into_iter()
			.map(|row| {
				let stats = stats.remove(&row.code).unwrap_or_default();
				let clicks = clicks.remove(&row.code).unwrap_or_default();
				view(row, &stats, clicks)
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<CreateBody>,
) -> Result<Json<ReferralView>, ReferralError> {
	let code = normalise_code(&body.code);
	if !valid_slug(&code.to_ascii_lowercase()) {
		return Err(ReferralError::InvalidCode);
	}
	ensure_coupon(&state, body.discount_id).await?;

	let model = referral::ActiveModel {
		code: Set(code.clone()),
		name: Set(body.name),
		discount_id: Set(body.discount_id),
		enabled: Set(body.enabled),
		created_at: ActiveValue::NotSet,
	}
	.insert(&state.database)
	.await
	.map_err(|error| match error.sql_err() {
		Some(SqlErr::UniqueConstraintViolation(_)) => ReferralError::DuplicateCode(code),
		_ => error.into(),
	})?;

	Ok(Json(view(model, &Stats::default(), 0)))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn update(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(code): Path<String>,
	Json(body): Json<UpdateBody>,
) -> Result<Json<ReferralView>, ReferralError> {
	ensure_coupon(&state, body.discount_id).await?;

	let mut model: referral::ActiveModel = Referral::find_by_id(normalise_code(&code))
		.one(&state.database)
		.await?
		.ok_or(ReferralError::NotFound)?
		.into();
	model.name = Set(body.name);
	model.discount_id = Set(body.discount_id);
	model.enabled = Set(body.enabled);
	let model = model.update(&state.database).await?;

	Ok(Json(view(model, &Stats::default(), 0)))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn remove(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(code): Path<String>,
) -> Result<StatusCode, ReferralError> {
	let result = Referral::delete_by_id(normalise_code(&code))
		.exec(&state.database)
		.await?;

	if result.rows_affected == 0 {
		return Err(ReferralError::NotFound);
	}

	Ok(StatusCode::NO_CONTENT)
}

fn view(model: referral::Model, stats: &Stats, clicks: i64) -> ReferralView {
	ReferralView {
		code: model.code,
		name: model.name,
		discount_id: model.discount_id,
		enabled: model.enabled,
		created_at: model.created_at.to_rfc3339(),
		orders: stats.orders,
		revenue_minor: stats.revenue_minor.unwrap_or_default(),
		clicks,
	}
}

/// A sale applies by itself, so only a coupon means anything here.
async fn ensure_coupon(
	state: &ApiState,
	discount_id: Option<i32>,
) -> Result<(), ReferralError> {
	let Some(id) = discount_id else {
		return Ok(());
	};

	match Discount::find_by_id(id).one(&state.database).await? {
		Some(discount) if discount.code.is_some() => Ok(()),
		_ => Err(ReferralError::NotACoupon(id)),
	}
}
