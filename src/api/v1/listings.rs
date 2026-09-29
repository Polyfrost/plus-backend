use std::collections::HashSet;

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset};
use entities::{cosmetic, prelude::*, product_requirement};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait as _, ActiveValue, TryIntoModel as _, ColumnTrait as _, EntityTrait, PaginatorTrait as _,
	QueryFilter as _, Set, TransactionTrait as _,
};
use serde::Deserialize;

use crate::{
	api::{ApiState, admin_auth::AdminAuthenticationExtractor},
	paynow::PayNowError,
	product_settings::{Key, SettingsInfo, infos, push},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SettingsError {
	#[error("No such cosmetic or bundle")]
	NotFound,
	#[error("{0}")]
	Invalid(&'static str),
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Saved, but PayNow rejected the update: {0}")]
	PayNow(#[from] PayNowError),
}

impl IntoResponse for SettingsError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::Invalid(_) => StatusCode::BAD_REQUEST,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
				Self::PayNow(_) => StatusCode::BAD_GATEWAY,
			},
			self,
		)
	}
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Kind {
	Cosmetics,
	Bundles,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SettingsBody {
	/// False takes it off sale and out of the store, without losing the
	/// other settings.
	#[serde(default = "enabled")]
	for_sale: bool,
	#[serde(default)]
	available_from: Option<DateTime<FixedOffset>>,
	#[serde(default)]
	available_until: Option<DateTime<FixedOffset>>,
	/// Units that can ever be sold, refunds excluded.
	#[serde(default)]
	stock_limit: Option<i32>,
	/// How many one player may buy, gifts included.
	#[serde(default)]
	customer_limit: Option<i32>,
	/// The window `customer_limit` counts over. Leave out for forever.
	#[serde(default)]
	customer_limit_days: Option<i32>,
	#[serde(default)]
	gifting_disabled: bool,
	/// Coupons never apply to this product. Sales still do.
	#[serde(default)]
	coupons_disabled: bool,
	/// Cosmetics the recipient must own first.
	#[serde(default)]
	requires: Vec<i32>,
	#[serde(default = "every_requirement")]
	requires_all: bool,
	/// Makes this a rental: owned for this many days from purchase. Buying
	/// it again extends it; a permanent copy is never affected.
	#[serde(default)]
	expires_after_days: Option<i32>,
}

fn enabled() -> bool {
	true
}

fn every_requirement() -> bool {
	true
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/admin/{kind}/{id}/listing",
		get_with(self::read, self::read_doc).put_with(self::replace, self::replace_doc),
	)
}

fn read_doc(op: TransformOperation) -> TransformOperation {
	op.id("readListing")
		.summary("Read a product's listing")
		.description(
			"Returns the sale window, stock and purchase limits, gifting, discount, \
			 requirement and rental settings of a cosmetic or bundle, or null when \
			 none are set. `kind` is `cosmetics` or `bundles`; a variant id reads \
			 its group's. Admin password required.",
		)
		.tag("listings")
}

fn replace_doc(op: TransformOperation) -> TransformOperation {
	op.id("replaceListing")
		.summary("Replace a product's listing")
		.description(
			"Replaces every setting of a cosmetic or bundle, enforced at checkout. \
			 A variant id sets its group's. `for_sale: false` hides it from the \
			 store and disables it on PayNow. The sale window, gifting and \
			 coupon settings are also pushed to PayNow; a 502 means they were saved here \
			 but not there, and `provision-paynow --sync-settings` repairs it. \
			 Admin password required.",
		)
		.tag("listings")
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn read(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path((kind, id)): Path<(Kind, i32)>,
) -> Result<Json<Option<SettingsInfo>>, SettingsError> {
	let key = key_of(&state, kind, id).await?;
	Ok(Json(
		infos(&state.database, &[key])
			.await?
			.remove(&key),
	))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn replace(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path((kind, id)): Path<(Kind, i32)>,
	Json(body): Json<SettingsBody>,
) -> Result<Json<Option<SettingsInfo>>, SettingsError> {
	let key = key_of(&state, kind, id).await?;
	validate(&state, &body).await?;

	let txn = state.database.begin().await?;

	let existing = ProductSettings::find()
		.filter(key.settings_filter())
		.one(&txn)
		.await?;
	let (cosmetic_id, cosmetic_group_id, bundle_id) = match key {
		Key::Cosmetic(id) => (Some(id), None, None),
		Key::Group(id) => (None, Some(id), None),
		Key::Bundle(id) => (None, None, Some(id)),
	};
	let active = entities::product_settings::ActiveModel {
		id: existing.map_or(ActiveValue::NotSet, |row| ActiveValue::Unchanged(row.id)),
		cosmetic_id: Set(cosmetic_id),
		cosmetic_group_id: Set(cosmetic_group_id),
		bundle_id: Set(bundle_id),
		available_from: Set(body.available_from),
		available_until: Set(body.available_until),
		stock_limit: Set(body.stock_limit),
		customer_limit: Set(body.customer_limit),
		customer_limit_days: Set(body.customer_limit_days),
		gifting_disabled: Set(body.gifting_disabled),
		coupons_disabled: Set(body.coupons_disabled),
		for_sale: Set(body.for_sale),
		requires_all: Set(body.requires_all),
		expires_after_days: Set(body.expires_after_days),
	};
	let model = active.save(&txn).await?.try_into_model()?;

	ProductRequirement::delete_many()
		.filter(product_requirement::Column::SettingsId.eq(model.id))
		.exec(&txn)
		.await?;
	let requirements: HashSet<i32> = body.requires.iter().copied().collect();
	if !requirements.is_empty() {
		ProductRequirement::insert_many(requirements.into_iter().map(|cosmetic_id| {
			product_requirement::ActiveModel {
				settings_id: Set(model.id),
				cosmetic_id: Set(cosmetic_id),
			}
		}))
		.exec_without_returning(&txn)
		.await?;
	}

	txn.commit().await?;

	if let Some(product_id) = key.product_id(&state.database).await? {
		push(&state.paynow.client, &product_id, &model).await?;
	}

	Ok(Json(
		infos(&state.database, &[key])
			.await?
			.remove(&key),
	))
}

async fn key_of(state: &ApiState, kind: Kind, id: i32) -> Result<Key, SettingsError> {
	match kind {
		Kind::Cosmetics => Cosmetic::find_by_id(id)
			.one(&state.database)
			.await?
			.map(|cosmetic| Key::of_cosmetic(&cosmetic)),
		Kind::Bundles => Bundles::find_by_id(id)
			.one(&state.database)
			.await?
			.map(|bundle| Key::from(&bundle)),
	}
	.ok_or(SettingsError::NotFound)
}

async fn validate(state: &ApiState, body: &SettingsBody) -> Result<(), SettingsError> {
	if let (Some(from), Some(until)) = (body.available_from, body.available_until)
		&& from >= until
	{
		return Err(SettingsError::Invalid(
			"available_from must be before available_until",
		));
	}
	if [
		body.stock_limit,
		body.customer_limit,
		body.customer_limit_days,
		body.expires_after_days,
	]
		.into_iter()
		.flatten()
		.any(|value| value < 1)
	{
		return Err(SettingsError::Invalid("Limits must be at least 1"));
	}
	if body.customer_limit_days.is_some() && body.customer_limit.is_none() {
		return Err(SettingsError::Invalid(
			"customer_limit_days needs customer_limit",
		));
	}

	let requirements: HashSet<i32> = body.requires.iter().copied().collect();
	if !requirements.is_empty() {
		let found = Cosmetic::find()
			.filter(cosmetic::Column::Id.is_in(requirements.iter().copied()))
			.count(&state.database)
			.await?;
		if found != requirements.len() as u64 {
			return Err(SettingsError::Invalid("A required cosmetic does not exist"));
		}
	}

	Ok(())
}
