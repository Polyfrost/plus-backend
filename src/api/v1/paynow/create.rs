use std::{collections::HashMap, net::IpAddr};

use aide::{OperationIo, operation::OperationInput, transform::TransformOperation};
use axum::{
	Json,
	extract::{FromRequestParts, State},
	http::{StatusCode, request::Parts},
	response::{IntoResponse, Response},
};
use axum_client_ip::ClientIp;
use entities::{player_owned_cosmetic, prelude::*, user};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, DbErr, Set, prelude::*};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};
use uuid::Uuid;

use super::resolve::{Product, dedupe};
use crate::{
	api::{ApiState, state::CHECKOUTS_PER_COOLDOWN},
	paynow::{PayNowError, checkouts::NewCheckout, models::CreateCheckoutLine},
};

/// Wrapped so aide leaves it out of the OpenAPI document. `None` when the
/// configured source cannot produce one: a sale is not worth failing over a
/// rate limit that cannot be applied.
pub(super) struct BuyerIp(Option<IpAddr>);

impl OperationInput for BuyerIp {
	fn operation_input(
		_ctx: &mut aide::generate::GenContext,
		_operation: &mut aide::openapi::Operation,
	) {
	}
}

impl FromRequestParts<ApiState> for BuyerIp {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		Ok(Self(
			ClientIp::from_request_parts(parts, state)
				.await
				.map(|ClientIp(ip)| ip)
				.inspect_err(|_| {
					warn!("Unable to resolve the buyer's address; not rate limiting")
				})
				.ok(),
		))
	}
}

/// A basket larger than this is a bug or an attack, not a purchase.
const MAX_CHECKOUT_LINES: usize = 25;
/// Nobody legitimately stacks more codes than this, and PayNow rejects the
/// whole checkout if any one of them is invalid.
const MAX_PROMO_CODES: usize = 5;

#[derive(Debug, thiserror::Error, OperationIo)]
pub(super) enum CreateError {
	#[error("Unable to create checkout: {0}")]
	PayNow(#[from] PayNowError),
	#[error("PayNow did not return a checkout url")]
	MissingUrl,
	#[error("Player already owns {0}")]
	AlreadyOwned(String),
	#[error("No products were requested")]
	NoProducts,
	#[error("A checkout may contain at most {MAX_CHECKOUT_LINES} products")]
	TooManyProducts,
	#[error("Unknown product {0}")]
	UnknownProduct(String),
	#[error("At most {MAX_PROMO_CODES} promo codes may be applied")]
	TooManyPromoCodes,
	#[error("{0}")]
	RejectedByProvider(String),
	#[error("Too many checkout attempts, try again in a minute")]
	RateLimited,
	#[error("Unable to check existing ownership: {0}")]
	Database(#[from] DbErr),
}

impl IntoResponse for CreateError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				CreateError::PayNow(_) => StatusCode::BAD_GATEWAY,
				CreateError::MissingUrl | CreateError::Database(_) => {
					StatusCode::INTERNAL_SERVER_ERROR
				}
				CreateError::AlreadyOwned(_) => StatusCode::CONFLICT,
				CreateError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
				CreateError::NoProducts
				| CreateError::TooManyProducts
				| CreateError::UnknownProduct(_)
				| CreateError::TooManyPromoCodes
				| CreateError::RejectedByProvider(_) => StatusCode::BAD_REQUEST,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct CreateRequest {
	/// The Minecraft UUID of the receiving player
	player: Uuid,
	/// The Minecraft UUID of the buyer, None if player == buyer
	buyer: Option<Uuid>,
	/// The storefront product ids to charge for, one checkout line each
	products: Vec<String>,
	/// Promo codes to apply. An invalid one fails the whole checkout, so the
	/// buyer is told which.
	#[serde(default)]
	promo_codes: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CreateResponse {
	/// The hosted checkout page url to redirect the buyer to
	url: String,
}

pub fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("createCheckout")
		.summary("Create a checkout")
		.description(concat!(
			"Creates a hosted checkout for one or more cosmetics/emotes using ",
			"the store product ids returned from the cosmetic and bundle view ",
			"endpoints. Responds 409 naming the cosmetics if the receiving ",
			"player already owns any of them, and 400 if a product id does not ",
			"resolve to an enabled cosmetic or bundle or a promo code is rejected."
		))
		.tag("checkout")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn endpoint(
	State(state): State<ApiState>,
	BuyerIp(ip): BuyerIp,
	Json(request): Json<CreateRequest>,
) -> Result<Json<CreateResponse>, CreateError> {
	let CreateRequest {
		player,
		products,
		buyer,
		promo_codes,
	} = request;
	let buyer = buyer.unwrap_or(player);

	enforce_rate_limit(&state, ip).await?;

	let promo_codes = dedupe(
		promo_codes
			.into_iter()
			.map(|code| code.trim().to_owned())
			.filter(|code| !code.is_empty())
			.collect(),
	);
	if promo_codes.len() > MAX_PROMO_CODES {
		return Err(CreateError::TooManyPromoCodes);
	}

	let products = dedupe(products);
	if products.is_empty() {
		return Err(CreateError::NoProducts);
	}
	if products.len() > MAX_CHECKOUT_LINES {
		return Err(CreateError::TooManyProducts);
	}

	let resolved =
		super::resolve::resolve_products(&state.database, &products, true).await?;

	// An unresolvable line would still be charged, then grant nothing.
	for product_id in &products {
		if !resolved.contains_key(product_id) {
			return Err(CreateError::UnknownProduct(product_id.clone()));
		}
	}

	let cosmetics: Vec<_> = resolved
		.values()
		.flat_map(Product::cosmetics)
		.cloned()
		.collect();
	reject_already_owned(&state, player, &cosmetics).await?;

	let buyer_customer = customer_id(&state, buyer).await?;
	let player_customer = if buyer == player {
		buyer_customer.clone()
	} else {
		customer_id(&state, player).await?
	};

	let lines = products
		.iter()
		.map(|product_id| CreateCheckoutLine {
			product_id: product_id.clone(),
			quantity: 1,
			// The checkout's customer is who pays; the line's is who receives.
			gift_to_customer_id: (buyer != player).then(|| player_customer.clone()),
		})
		.collect();

	let metadata = HashMap::from([
		("player".to_string(), player.to_string()),
		("buyer".to_string(), buyer.to_string()),
		("products".to_string(), products.join(",")),
	]);

	let session = state
		.paynow
		.client
		.create_checkout(NewCheckout {
			customer_id: &buyer_customer,
			lines,
			promo_codes,
			return_url: &state.paynow.return_url,
			cancel_url: &state.paynow.cancel_url,
			metadata,
			customer_ip: ip,
		})
		.await
		// Everything else was validated first, so PayNow blaming the request
		// means a promo code the buyer can correct.
		.map_err(|error| match error.message() {
			Some(message) if error.is_client_error() => {
				CreateError::RejectedByProvider(message.to_owned())
			}
			_ => CreateError::PayNow(error),
		})?;

	session
		.url
		.map(|url| Json(CreateResponse { url }))
		.ok_or(CreateError::MissingUrl)
}

/// The endpoint is unauthenticated and creates a storefront customer as a
/// side effect, so one address cannot be allowed to hammer it.
async fn enforce_rate_limit(
	state: &ApiState,
	ip: Option<IpAddr>,
) -> Result<(), CreateError> {
	let Some(ip) = ip else {
		return Ok(());
	};

	let attempts = state
		.checkout_cooldown
		.get(&ip)
		.await
		.unwrap_or(0)
		.saturating_add(1);
	state.checkout_cooldown.insert(ip, attempts).await;

	if attempts > CHECKOUTS_PER_COOLDOWN {
		return Err(CreateError::RateLimited);
	}

	Ok(())
}

async fn reject_already_owned(
	state: &ApiState,
	player: Uuid,
	cosmetics: &[entities::cosmetic::Model],
) -> Result<(), CreateError> {
	if cosmetics.is_empty() {
		return Ok(());
	}

	let Some(user) = User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.one(&state.database)
		.await?
	else {
		return Ok(());
	};

	let owned: Vec<i32> = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.eq(user.id))
		.filter(
			player_owned_cosmetic::Column::CosmeticId
				.is_in(cosmetics.iter().map(|cosmetic| cosmetic.id)),
		)
		.all(&state.database)
		.await?
		.into_iter()
		.map(|owned| owned.cosmetic_id)
		.collect();

	if owned.is_empty() {
		return Ok(());
	}

	Err(CreateError::AlreadyOwned(
		cosmetics
			.iter()
			.filter(|cosmetic| owned.contains(&cosmetic.id))
			.map(super::resolve::display_name)
			.collect::<Vec<_>>()
			.join(", "),
	))
}

/// Cached on the user row so a repeat checkout skips the lookup.
async fn customer_id(state: &ApiState, player: Uuid) -> Result<String, CreateError> {
	let user = User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.one(&state.database)
		.await?;

	if let Some(user) = &user
		&& let Some(customer_id) = &user.paynow_customer_id
	{
		return Ok(customer_id.clone());
	}

	let customer = state
		.paynow
		.client
		.get_or_create_customer(player, user.as_ref().and_then(|u| u.username.as_deref()))
		.await?;

	if let Some(user) = user {
		let mut update: user::ActiveModel = user.into();
		update.paynow_customer_id = Set(Some(customer.id.clone()));
		if let Err(error) = update.update(&state.database).await {
			// Only a cache, so a failed write costs a lookup rather than a sale.
			error!("Unable to store PayNow customer id: {error}");
		}
	}

	Ok(customer.id)
}
