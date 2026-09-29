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
	paynow::{PayNowError, models::UpsertProduct},
	utils::money::to_cents,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum UpdateError {
	#[error("The requested bundle does not exist")]
	MissingBundle,
	#[error("The bundle has no storefront product to price")]
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
				Self::MissingBundle => StatusCode::NOT_FOUND,
				Self::MissingProduct => StatusCode::BAD_REQUEST,
				Self::PayNow(_) => StatusCode::BAD_GATEWAY,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

/// The pricing columns a request resolves to.
struct PriceUpdate {
	store_product_id: String,
	base_price: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateRequest {
	/// The id of the bundle to update.
	bundle_id: i32,
	/// When set, toggles the enabled flag.
	enabled: Option<bool>,
	/// When set, renames the bundle.
	name: Option<String>,
	/// When present, sets (or clears with null) the collection.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	collection: Option<Option<i32>>,
	/// When present, sets (or clears with null) the description.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	description: Option<Option<String>>,
	/// When present, replaces the bundle's contained cosmetics with this set.
	cosmetic_ids: Option<Vec<i32>>,
	/// The bundle's list price in USD major units. Sales and coupons are run
	/// from `/v1/admin/discounts`, not from here.
	new_price: Option<f32>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateBundle")
		.summary("Update a bundle")
		.description(
			"Updates a bundle's metadata (enabled, name, collection, description), \
			 optionally replaces its contained cosmetics, and sets its list price on \
			 PayNow. Discounts are not set here: run a sale or a coupon from \
			 `/v1/admin/discounts`. Admin password required.",
		)
		.tag("bundles")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The bundle was updated")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No bundle exists with the given id")
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
	use entities::{bundles, bundles_cosmetics, prelude::*};

	let Some(bundle) = Bundles::find_by_id(body.bundle_id)
		.one(&state.database)
		.await?
	else {
		return Err(UpdateError::MissingBundle);
	};

	// Resolved without calling PayNow yet: its price is a destructive patch,
	// so the database has to commit first.
	let price_update = match body.new_price {
		Some(new_price) => Some(PriceUpdate {
			store_product_id: bundle
				.store_product_id
				.clone()
				.ok_or(UpdateError::MissingProduct)?,
			base_price: new_price,
		}),
		None => None,
	};
	let visibility_product = bundle.store_product_id.clone();

	let txn = state.database.begin().await?;

	let mut active: bundles::ActiveModel = bundle.into();
	let mut changed = false;

	if let Some(name) = &body.name {
		active.name = Set(name.clone());
		changed = true;
	}
	if let Some(enabled) = body.enabled {
		active.enabled = Set(enabled);
		changed = true;
	}
	if let Some(collection) = &body.collection {
		active.collection = Set(*collection);
		changed = true;
	}
	if let Some(description) = &body.description {
		active.description = Set(description.clone());
		changed = true;
	}
	if let Some(price) = &price_update {
		active.base_price = Set(Some(price.base_price));
		changed = true;
	}

	if changed {
		active.update(&txn).await?;
	}

	// Replace the bundle's contents when a new set was provided.
	if let Some(cosmetic_ids) = &body.cosmetic_ids {
		BundlesCosmetics::delete_many()
			.filter(bundles_cosmetics::Column::BundleId.eq(body.bundle_id))
			.exec(&txn)
			.await?;

		if !cosmetic_ids.is_empty() {
			BundlesCosmetics::insert_many(cosmetic_ids.iter().map(|cosmetic_id| {
				bundles_cosmetics::ActiveModel {
					bundle_id: Set(body.bundle_id),
					cosmetic_id: Set(*cosmetic_id),
				}
			}))
			.on_conflict_do_nothing()
			.exec(&txn)
			.await?;
		}
	}

	txn.commit().await?;

	// Only once the database has committed. Drift from a failure here is
	// repaired by `provision-paynow --sync-prices`.
	if let Some(price) = &price_update {
		state
			.paynow
			.client
			.set_product_price(&price.store_product_id, to_cents(price.base_price))
			.await?;
	}

	if let Some(enabled) = body.enabled
		&& let Some(product_id) = visibility_product
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

	// A new collection changes which PayNow sales and coupons reach it.
	if body.collection.is_some() {
		storefront::sync_bundle_tags_or_warn(
			&state.database,
			&state.paynow.client,
			&[body.bundle_id],
		)
		.await;
	}

	Ok(StatusCode::NO_CONTENT)
}
