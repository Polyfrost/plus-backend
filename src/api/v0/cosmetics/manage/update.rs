use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait,
};
use serde::Deserialize;

use crate::{
	api::{ApiState, admin_auth::AdminAuthenticationExtractor},
	paynow::{PayNowError, catalog, models::UpsertProduct},
	utils::money::{discounted, to_cents},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum UpdateError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("The cosmetic has no storefront product to price")]
	MissingProduct,
	#[error("The cosmetic has no base price to discount from")]
	MissingBasePrice,
	#[error("A discount requires either a discount rate or a new price")]
	InvalidDiscount,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error("PayNow error: {0}")]
	PayNow(#[from] PayNowError),
}

impl IntoResponse for UpdateError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::MissingProduct | Self::MissingBasePrice | Self::InvalidDiscount => {
					StatusCode::BAD_REQUEST
				}
				Self::PayNow(_) => StatusCode::BAD_GATEWAY,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

/// The pricing columns a request resolves to, applied to every affected row.
struct PriceUpdate {
	store_product_id: String,
	/// What PayNow should charge once the change lands, in minor units.
	effective_minor: i64,
	/// Set only on a silent increase; left untouched for a discount.
	base_price: Option<f32>,
	/// Always written: the rate for a discount, `None` to clear the discount on
	/// a silent increase (which restores the full default price).
	discount_rate: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateRequest {
	/// The id of the cosmetic (or any of its variants) to update.
	cosmetic_id: i32,
	/// When set, toggles the enabled flag (of the group when grouped).
	enabled: Option<bool>,
	/// When set, renames the cosmetic (the group when grouped).
	name: Option<String>,
	/// When present, sets (or clears with null) the collection on every variant.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	collection: Option<Option<i32>>,
	/// When present, sets (or clears with null) the description on every variant.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	description: Option<Option<String>>,
	/// A new price in USD major units. Without `discount` this is a silent
	/// increase; with `discount` it is the discounted price.
	new_price: Option<f32>,
	/// Whether this update creates a discount rather than a silent price change.
	#[serde(default)]
	discount: bool,
	/// The discount percentage. Optional when `new_price` is given (then it is
	/// computed); required otherwise.
	discount_rate: Option<i32>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateCosmetic")
		.summary("Update a cosmetic")
		.description(
			"Updates a cosmetic's metadata (enabled, name, collection, \
			 description) and drives its Stripe pricing. A silent price increase \
			 creates a new default price, provisioning the Stripe product first \
			 when the cosmetic was uploaded without a price; a discount creates a \
			 non-default price and records the rate, and requires an already \
			 priced cosmetic. For a grouped cosmetic, name/enabled apply to the \
			 group and price changes propagate to every variant. Admin password \
			 required.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The cosmetic was updated")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No cosmetic exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/update", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<UpdateRequest>,
) -> Result<StatusCode, UpdateError> {
	use entities::{cosmetic, cosmetic_group, prelude::*};

	let Some(cosmetic) = Cosmetic::find_by_id(body.cosmetic_id)
		.one(&state.database)
		.await?
	else {
		return Err(UpdateError::MissingCosmetic);
	};

	let existing_product = match cosmetic.store_product_id.clone() {
		Some(product_id) => Some(product_id),
		None => match cosmetic.group_id {
			Some(group_id) => Cosmetic::find()
				.filter(cosmetic::Column::GroupId.eq(group_id))
				.filter(cosmetic::Column::StoreProductId.is_not_null())
				.one(&state.database)
				.await?
				.and_then(|sibling| sibling.store_product_id),
			None => None,
		},
	};
	let visibility_product = existing_product.clone();

	// Resolved without calling PayNow yet: its price is a destructive patch,
	// so the database has to commit first.
	let price_update = if body.new_price.is_some() || body.discount {
		if body.discount {
			let product_id = existing_product.ok_or(UpdateError::MissingProduct)?;
			let base = cosmetic.base_price.ok_or(UpdateError::MissingBasePrice)?;
			let (price, rate) = match (body.discount_rate, body.new_price) {
				(Some(rate), _) => (discounted(base, rate), rate),
				(None, Some(new_price)) => {
					let rate = (((base - new_price) / base) * 100.0).round() as i32;
					(new_price, rate)
				}
				(None, None) => return Err(UpdateError::InvalidDiscount),
			};

			Some(PriceUpdate {
				store_product_id: product_id,
				effective_minor: to_cents(price),
				base_price: None,
				discount_rate: Some(rate),
			})
		} else {
			// Silent increase: new_price is guaranteed present by the guard above.
			let new_price = body.new_price.ok_or(UpdateError::InvalidDiscount)?;

			let product_id = match existing_product {
				Some(product_id) => product_id,
				None => {
					let group_name = match cosmetic.group_id {
						Some(group_id) => CosmeticGroup::find_by_id(group_id)
							.one(&state.database)
							.await?
							.map(|group| group.name),
						None => None,
					};
					let product_name = body
						.name
						.clone()
						.or(group_name)
						.or_else(|| cosmetic.name.clone())
						.ok_or(UpdateError::MissingProduct)?;
					let description = match &body.description {
						Some(description) => description.clone(),
						None => cosmetic.description.clone(),
					};
					let slug = match cosmetic.group_id {
						Some(group_id) => catalog::cosmetic_group_slug(group_id),
						None => catalog::cosmetic_slug(cosmetic.id),
					};

					// Additive, so safe before the transaction.
					state
						.paynow
						.client
						.create_product(
							&slug,
							&product_name,
							description.as_deref(),
							to_cents(new_price),
							!cosmetic.enabled,
						)
						.await?
				}
			};

			Some(PriceUpdate {
				store_product_id: product_id,
				effective_minor: to_cents(new_price),
				base_price: Some(new_price),
				discount_rate: None,
			})
		}
	} else {
		None
	};

	let txn = state.database.begin().await?;

	// Grouped cosmetics carry name/enabled on the group; ungrouped ones on the
	// row itself (handled below with the other row-level columns).
	if let Some(group_id) = cosmetic.group_id
		&& (body.name.is_some() || body.enabled.is_some())
		&& let Some(group) = CosmeticGroup::find_by_id(group_id).one(&txn).await?
	{
		let mut active: cosmetic_group::ActiveModel = group.into();
		if let Some(name) = &body.name {
			active.name = Set(name.clone());
		}
		if let Some(enabled) = body.enabled {
			active.enabled = Set(enabled);
		}
		active.update(&txn).await?;
	}

	// Apply collection/description/price to every affected row, plus name/enabled
	// for ungrouped cosmetics.
	let rows = match cosmetic.group_id {
		Some(group_id) => {
			Cosmetic::find()
				.filter(cosmetic::Column::GroupId.eq(group_id))
				.all(&txn)
				.await?
		}
		None => vec![cosmetic.clone()],
	};

	for row in rows {
		let is_grouped = row.group_id.is_some();
		let mut active: cosmetic::ActiveModel = row.into();
		let mut changed = false;

		if let Some(collection) = &body.collection {
			active.collection = Set(*collection);
			changed = true;
		}
		if let Some(description) = &body.description {
			active.description = Set(description.clone());
			changed = true;
		}
		if let Some(price) = &price_update {
			active.store_product_id = Set(Some(price.store_product_id.clone()));
			if let Some(base) = price.base_price {
				active.base_price = Set(Some(base));
			}
			active.discount_rate = Set(price.discount_rate);
			changed = true;
		}
		if !is_grouped {
			if let Some(name) = &body.name {
				active.name = Set(Some(name.clone()));
				changed = true;
			}
			if let Some(enabled) = body.enabled {
				active.enabled = Set(enabled);
				changed = true;
			}
		}

		if changed {
			active.update(&txn).await?;
		}
	}

	txn.commit().await?;

	if let Some(price) = &price_update {
		state
			.paynow
			.client
			.set_product_price(&price.store_product_id, price.effective_minor)
			.await?;
	}

	if let Some(enabled) = body.enabled
		&& let Some(product_id) = price_update
			.as_ref()
			.map(|price| price.store_product_id.clone())
			.or(visibility_product)
	{
		state
			.paynow
			.client
			.update_product(
				&product_id,
				&UpsertProduct {
					is_hidden: Some(!enabled),
					..Default::default()
				},
			)
			.await?;
	}

	Ok(StatusCode::NO_CONTENT)
}
