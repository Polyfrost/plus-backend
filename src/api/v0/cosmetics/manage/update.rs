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
	storefront,
	paynow::{PayNowError, catalog, models::UpsertProduct},
	utils::money::to_cents,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum UpdateError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("The cosmetic has no storefront product to price")]
	MissingProduct,
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
				Self::MissingProduct => StatusCode::BAD_REQUEST,
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
	base_price: f32,
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
	/// The cosmetic's list price in USD major units. Sales and coupons are run
	/// from `/v1/admin/discounts`, not from here.
	new_price: Option<f32>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateCosmetic")
		.summary("Update a cosmetic")
		.description(
			"Updates a cosmetic's metadata (enabled, name, collection, \
			 description) and its list price on PayNow, provisioning the product \
			 first when the cosmetic was uploaded without a price. Discounts are \
			 not set here: run a sale or a coupon from `/v1/admin/discounts`. \
			 For a grouped cosmetic, name/enabled apply to \
			 the group and price changes propagate to every variant. Admin \
			 password required.",
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
	let price_update = match body.new_price {
		Some(new_price) => {
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
				base_price: new_price,
			})
		}
		None => None,
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
			active.base_price = Set(Some(price.base_price));
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
			.set_product_price(&price.store_product_id, to_cents(price.base_price))
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

	// A new collection, or a product that did not exist until now, changes
	// which PayNow sales and coupons reach it.
	if body.collection.is_some() || price_update.is_some() {
		storefront::sync_cosmetic_tags_or_warn(
			&state.database,
			&state.paynow.client,
			&[body.cosmetic_id],
		)
		.await;
	}

	Ok(StatusCode::NO_CONTENT)
}
