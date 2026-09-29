//! Admin CRUD for the sales and coupons the backend prices with.
//!
//! One table, told apart by `code`: a row without one runs by itself (a sale),
//! a row with one has to be typed in (a coupon).

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
	extract::{Path, Query, State},
	http::StatusCode,
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset};
use entities::{discount, discount_target, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set,
	TransactionTrait,
};
use serde::{Deserialize, Serialize};

use crate::{
	api::{ApiState, admin_auth::AdminAuthenticationExtractor},
	pricing::{Targets, normalise_code},
	storefront,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum DiscountError {
	#[error("No discount with that id exists")]
	NotFound,
	#[error("A discount is either a percentage or a fixed amount, not both")]
	AmbiguousAmount,
	#[error("A fixed amount needs the currency it is denominated in")]
	MissingCurrency,
	#[error("A percentage must be between 1 and 100")]
	BadPercentage,
	#[error("A discount that targets nothing needs applies_to_all")]
	NoTargets,
	#[error("The code {0} is already in use")]
	DuplicateCode(String),
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("Saved, but not on PayNow: {0}")]
	Storefront(#[from] crate::storefront::SyncError),
	#[error("PayNow refused to delete it, so it was kept: {0}")]
	PayNow(#[from] crate::paynow::PayNowError),
}

impl IntoResponse for DiscountError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::NotFound => StatusCode::NOT_FOUND,
				Self::DuplicateCode(_) => StatusCode::CONFLICT,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
				Self::Storefront(_) | Self::PayNow(_) => StatusCode::BAD_GATEWAY,
				_ => StatusCode::BAD_REQUEST,
			},
			self,
		)
	}
}

impl Targets {
	fn rows(&self, discount_id: i32) -> Vec<discount_target::ActiveModel> {
		let blank = || discount_target::ActiveModel {
			id: ActiveValue::NotSet,
			discount_id: Set(discount_id),
			collection_id: Set(None),
			tag_id: Set(None),
			cosmetic_id: Set(None),
			cosmetic_group_id: Set(None),
			bundle_id: Set(None),
		};

		let mut rows = Vec::new();
		for id in &self.collections {
			rows.push(discount_target::ActiveModel {
				collection_id: Set(Some(*id)),
				..blank()
			});
		}
		for id in &self.tags {
			rows.push(discount_target::ActiveModel {
				tag_id: Set(Some(*id)),
				..blank()
			});
		}
		for id in &self.cosmetics {
			rows.push(discount_target::ActiveModel {
				cosmetic_id: Set(Some(*id)),
				..blank()
			});
		}
		for id in &self.cosmetic_groups {
			rows.push(discount_target::ActiveModel {
				cosmetic_group_id: Set(Some(*id)),
				..blank()
			});
		}
		for id in &self.bundles {
			rows.push(discount_target::ActiveModel {
				bundle_id: Set(Some(*id)),
				..blank()
			});
		}

		rows
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiscountBody {
	pub name: String,
	#[serde(default)]
	pub description: Option<String>,
	/// Omit for a sale, which runs by itself. Set for a coupon; stored and
	/// matched upper case.
	#[serde(default)]
	pub code: Option<String>,
	/// Whole percent off. Exactly one of this and `amount_off_minor`.
	#[serde(default)]
	pub percent_off: Option<i32>,
	#[serde(default)]
	pub amount_off_minor: Option<i64>,
	/// Required with `amount_off_minor`: a fixed amount only applies to lines
	/// priced in the same currency, which is what keeps it off a future
	/// virtual-currency balance.
	#[serde(default)]
	pub currency: Option<String>,
	#[serde(default)]
	pub starts_at: Option<DateTime<FixedOffset>>,
	#[serde(default)]
	pub ends_at: Option<DateTime<FixedOffset>>,
	#[serde(default = "enabled_by_default")]
	pub enabled: bool,
	/// Covers the whole catalogue, ignoring `targets`.
	#[serde(default)]
	pub applies_to_all: bool,
	#[serde(default)]
	pub targets: Targets,
	#[serde(default)]
	pub min_subtotal_minor: Option<i64>,
	#[serde(default)]
	pub max_redemptions: Option<i32>,
	#[serde(default)]
	pub max_per_player: Option<i32>,
}

fn enabled_by_default() -> bool {
	true
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiscountView {
	pub id: i32,
	pub name: String,
	pub description: Option<String>,
	pub code: Option<String>,
	pub percent_off: Option<i32>,
	pub amount_off_minor: Option<i64>,
	pub currency: Option<String>,
	pub starts_at: Option<String>,
	pub ends_at: Option<String>,
	pub enabled: bool,
	pub applies_to_all: bool,
	pub targets: Targets,
	pub min_subtotal_minor: Option<i64>,
	pub max_redemptions: Option<i32>,
	pub max_per_player: Option<i32>,
	pub redemptions: i32,
	/// The sale or coupon mirroring this on PayNow. Null until pushed, and
	/// for a fixed amount in a currency PayNow does not sell in.
	pub paynow_id: Option<String>,
	pub created_at: String,
}

fn view(model: discount::Model, targets: Targets) -> DiscountView {
	DiscountView {
		id: model.id,
		name: model.name,
		description: model.description,
		code: model.code,
		percent_off: model.percent_off,
		amount_off_minor: model.amount_off_minor,
		currency: model.currency,
		starts_at: model.starts_at.map(|at| at.to_rfc3339()),
		ends_at: model.ends_at.map(|at| at.to_rfc3339()),
		enabled: model.enabled,
		applies_to_all: model.applies_to_all,
		targets,
		min_subtotal_minor: model.min_subtotal_minor,
		max_redemptions: model.max_redemptions,
		max_per_player: model.max_per_player,
		redemptions: model.redemptions,
		paynow_id: model.paynow_id,
		created_at: model.created_at.to_rfc3339(),
	}
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListFilter {
	/// `sale` for the automatic ones, `coupon` for the coded ones. Both when
	/// left out.
	#[serde(default)]
	kind: Option<Kind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
	Sale,
	Coupon,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/admin/discounts",
			get_with(self::list, self::list_doc).post_with(self::create, self::create_doc),
		)
		.api_route(
			"/admin/discounts/{id}",
			post_with(self::update, self::update_doc)
				.delete_with(self::remove, self::remove_doc),
		)
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listDiscounts")
		.summary("List sales and coupons")
		.description(
			"Lists the discounts the backend prices with, newest first. These \
			 drive the sale price the client mod shows and the coupon codes \
			 checkout accepts. Admin password required.",
		)
		.tag("discounts")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createDiscount")
		.summary("Create a sale or coupon")
		.description(
			"Creates a discount. Leave `code` out for a sale, which applies on \
			 its own and shows on the storefront; set it for a coupon, which \
			 only applies when the buyer enters it.\n\nGive exactly one of \
			 `percent_off` and `amount_off_minor`; a fixed amount also needs \
			 `currency`, because it only applies to lines priced in that \
			 currency. Set `applies_to_all` or at least one target.\n\n\
			 Each item gets its best sale, then every coupon the buyer entered, \
			 each on what the last left.\n\nEvery sale and coupon is also \
			 created on PayNow, so its store shows and charges it too; a 502 \
			 means it was saved here but not there, and `provision-paynow \
			 --sync-discounts` repairs it. Admin password required.",
		)
		.tag("discounts")
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateDiscount")
		.summary("Replace a sale or coupon")
		.description(
			"Replaces every field of a discount, targets included. \
			 `redemptions` is not resettable. Admin password required.",
		)
		.tag("discounts")
}

fn remove_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteDiscount")
		.summary("Delete a sale or coupon")
		.description(
			"Deletes a discount and its targets. The redemption trail goes with \
			 it, so prefer disabling one that has already been used. Admin \
			 password required.",
		)
		.tag("discounts")
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn list(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Query(filter): Query<ListFilter>,
) -> Result<Json<Vec<DiscountView>>, DiscountError> {
	let mut query = Discount::find().order_by_desc(discount::Column::Id);
	query = match filter.kind {
		Some(Kind::Sale) => query.filter(discount::Column::Code.is_null()),
		Some(Kind::Coupon) => query.filter(discount::Column::Code.is_not_null()),
		None => query,
	};

	let rows = query.all(&state.database).await?;
	let mut targets = targets_for(&state, rows.iter().map(|row| row.id)).await?;

	Ok(Json(
		rows.into_iter()
			.map(|row| {
				let own = targets.remove(&row.id).unwrap_or_default();
				view(row, own)
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<DiscountBody>,
) -> Result<Json<DiscountView>, DiscountError> {
	validate(&body)?;
	let code = body.code.as_deref().map(normalise_code);
	reject_duplicate_code(&state, code.as_deref(), None).await?;

	let txn = state.database.begin().await?;

	let model = Discount::insert(active(&body, code, ActiveValue::NotSet))
		.exec_with_returning(&txn)
		.await?;

	let rows = body.targets.rows(model.id);
	if !rows.is_empty() {
		DiscountTarget::insert_many(rows)
			.exec_without_returning(&txn)
			.await?;
	}

	txn.commit().await?;

	let model = mirror(&state, None, model, &body.targets).await?;
	Ok(Json(view(model, body.targets)))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn update(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(id): Path<i32>,
	Json(body): Json<DiscountBody>,
) -> Result<Json<DiscountView>, DiscountError> {
	validate(&body)?;
	let previous = Discount::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(DiscountError::NotFound)?;

	let code = body.code.as_deref().map(normalise_code);
	reject_duplicate_code(&state, code.as_deref(), Some(id)).await?;

	let txn = state.database.begin().await?;

	let model = Discount::update(active(&body, code, ActiveValue::Unchanged(id)))
		.exec(&txn)
		.await?;

	// Replaced wholesale: a diff would have to guess which of five target
	// kinds the dashboard meant to drop.
	DiscountTarget::delete_many()
		.filter(discount_target::Column::DiscountId.eq(id))
		.exec(&txn)
		.await?;
	let rows = body.targets.rows(id);
	if !rows.is_empty() {
		DiscountTarget::insert_many(rows)
			.exec_without_returning(&txn)
			.await?;
	}

	txn.commit().await?;

	let model = mirror(&state, Some(&previous), model, &body.targets).await?;
	Ok(Json(view(model, body.targets)))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn remove(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Path(id): Path<i32>,
) -> Result<StatusCode, DiscountError> {
	let model = Discount::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(DiscountError::NotFound)?;

	// PayNow first: a row left behind can be deleted again, a sale left
	// running on PayNow would keep charging.
	storefront::delete_discount(&state.paynow.client, &model).await?;
	Discount::delete_by_id(id).exec(&state.database).await?;

	Ok(StatusCode::NO_CONTENT)
}

/// Saved first, then pushed: a PayNow failure leaves the row for
/// `provision-paynow --sync-discounts` to finish.
async fn mirror(
	state: &ApiState,
	previous: Option<&discount::Model>,
	model: discount::Model,
	targets: &Targets,
) -> Result<discount::Model, DiscountError> {
	Ok(storefront::push_and_store(
		&state.database,
		&state.paynow.client,
		previous,
		model,
		targets,
	)
	.await?)
}

fn active(
	body: &DiscountBody,
	code: Option<String>,
	id: ActiveValue<i32>,
) -> discount::ActiveModel {
	discount::ActiveModel {
		id,
		name: Set(body.name.clone()),
		description: Set(body.description.clone()),
		code: Set(code),
		percent_off: Set(body.percent_off),
		amount_off_minor: Set(body.amount_off_minor),
		currency: Set(body.currency.clone()),
		starts_at: Set(body.starts_at),
		ends_at: Set(body.ends_at),
		enabled: Set(body.enabled),
		applies_to_all: Set(body.applies_to_all),
		min_subtotal_minor: Set(body.min_subtotal_minor),
		max_redemptions: Set(body.max_redemptions),
		max_per_player: Set(body.max_per_player),
		redemptions: ActiveValue::NotSet,
		paynow_id: ActiveValue::NotSet,
		created_at: ActiveValue::NotSet,
	}
}

/// The same rules the database check constraint enforces, reported as a
/// readable error rather than a constraint violation.
fn validate(body: &DiscountBody) -> Result<(), DiscountError> {
	match (body.percent_off, body.amount_off_minor) {
		(Some(percent), None) => {
			if !(1..=100).contains(&percent) {
				return Err(DiscountError::BadPercentage);
			}
		}
		(None, Some(_)) => {
			if body.currency.as_deref().is_none_or(str::is_empty) {
				return Err(DiscountError::MissingCurrency);
			}
		}
		_ => return Err(DiscountError::AmbiguousAmount),
	}

	if !body.applies_to_all && body.targets.is_empty() {
		return Err(DiscountError::NoTargets);
	}

	Ok(())
}

async fn reject_duplicate_code(
	state: &ApiState,
	code: Option<&str>,
	ignoring: Option<i32>,
) -> Result<(), DiscountError> {
	let Some(code) = code else {
		return Ok(());
	};

	let mut query = Discount::find().filter(discount::Column::Code.eq(code));
	if let Some(id) = ignoring {
		query = query.filter(discount::Column::Id.ne(id));
	}

	if query.one(&state.database).await?.is_some() {
		return Err(DiscountError::DuplicateCode(code.to_owned()));
	}

	Ok(())
}

async fn targets_for(
	state: &ApiState,
	ids: impl Iterator<Item = i32>,
) -> Result<std::collections::HashMap<i32, Targets>, DiscountError> {
	let ids: Vec<i32> = ids.collect();
	if ids.is_empty() {
		return Ok(std::collections::HashMap::new());
	}

	let mut targets: std::collections::HashMap<i32, Targets> =
		std::collections::HashMap::new();
	for row in DiscountTarget::find()
		.filter(discount_target::Column::DiscountId.is_in(ids))
		.all(&state.database)
		.await?
	{
		let entry = targets.entry(row.discount_id).or_default();
		if let Some(id) = row.collection_id {
			entry.collections.push(id);
		}
		if let Some(id) = row.tag_id {
			entry.tags.push(id);
		}
		if let Some(id) = row.cosmetic_id {
			entry.cosmetics.push(id);
		}
		if let Some(id) = row.cosmetic_group_id {
			entry.cosmetic_groups.push(id);
		}
		if let Some(id) = row.bundle_id {
			entry.bundles.push(id);
		}
	}

	Ok(targets)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn body() -> DiscountBody {
		DiscountBody {
			name: "Winter".to_owned(),
			description: None,
			code: None,
			percent_off: Some(25),
			amount_off_minor: None,
			currency: None,
			starts_at: None,
			ends_at: None,
			enabled: true,
			applies_to_all: true,
			targets: Targets::default(),
			min_subtotal_minor: None,
			max_redemptions: None,
			max_per_player: None,
		}
	}

	#[test]
	fn a_percentage_sale_is_accepted() {
		assert!(validate(&body()).is_ok());
	}

	#[test]
	fn exactly_one_amount_is_required() {
		let mut both = body();
		both.amount_off_minor = Some(100);
		assert!(matches!(
			validate(&both),
			Err(DiscountError::AmbiguousAmount)
		));

		let mut neither = body();
		neither.percent_off = None;
		assert!(matches!(
			validate(&neither),
			Err(DiscountError::AmbiguousAmount)
		));
	}

	#[test]
	fn a_fixed_amount_needs_its_currency() {
		let mut fixed = body();
		fixed.percent_off = None;
		fixed.amount_off_minor = Some(200);
		assert!(matches!(
			validate(&fixed),
			Err(DiscountError::MissingCurrency)
		));

		fixed.currency = Some("usd".to_owned());
		assert!(validate(&fixed).is_ok());
	}

	#[test]
	fn a_percentage_stays_a_percentage() {
		for percent in [0, 101, -5] {
			let mut bad = body();
			bad.percent_off = Some(percent);
			assert!(
				matches!(validate(&bad), Err(DiscountError::BadPercentage)),
				"{percent} should be rejected"
			);
		}
	}

	#[test]
	fn a_discount_has_to_cover_something() {
		let mut scoped = body();
		scoped.applies_to_all = false;
		assert!(matches!(validate(&scoped), Err(DiscountError::NoTargets)));

		scoped.targets.collections = vec![1];
		assert!(validate(&scoped).is_ok());
	}
}
