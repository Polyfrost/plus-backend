use std::{
	collections::{HashMap, HashSet},
	net::IpAddr,
};

use aide::{OperationIo, operation::OperationInput, transform::TransformOperation};
use axum::{
	Json,
	extract::{FromRequestParts, State},
	http::{StatusCode, request::Parts},
	response::{IntoResponse, Response},
};
use axum_client_ip::ClientIp;
use chrono::Utc;
use entities::{player_owned_cosmetic, prelude::*, tags_cosmetic, user};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, DbErr, Set, prelude::*};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};
use uuid::Uuid;

use super::resolve::{Product, dedupe};
use crate::{
	api::{ApiState, rate_limit::address_key},
	paynow::{PayNowError, checkouts::NewCheckout, models::CreateCheckoutLine},
	pricing::{Rule, Sellable, live_rules, normalise_code, quote},
	product_settings::{self, Key, Settings},
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
	#[error("Unknown or expired promo code {0}")]
	UnknownPromoCode(String),
	#[error("{0} is not on sale right now")]
	NotAvailable(String),
	#[error("{0} cannot be gifted")]
	GiftingDisabled(String),
	#[error("The player does not own what {0} requires")]
	MissingRequirement(String),
	#[error("{0} is sold out")]
	SoldOut(String),
	#[error("The buyer has already bought {0} as many times as allowed")]
	LimitReached(String),
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
				CreateError::AlreadyOwned(_)
				| CreateError::SoldOut(_)
				| CreateError::LimitReached(_) => StatusCode::CONFLICT,
				CreateError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
				CreateError::NoProducts
				| CreateError::TooManyProducts
				| CreateError::UnknownProduct(_)
				| CreateError::TooManyPromoCodes
				| CreateError::UnknownPromoCode(_)
				| CreateError::NotAvailable(_)
				| CreateError::GiftingDisabled(_)
				| CreateError::MissingRequirement(_)
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
			"player already owns any of them or a product is sold out or at the ",
			"buyer's limit, and 400 if a product id does not resolve to an ",
			"enabled cosmetic or bundle, is outside its sale window, cannot be ",
			"gifted, needs a cosmetic the player lacks, or a promo code is rejected."
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

	let keys: Vec<Key> = resolved.values().map(Product::key).collect();
	let settings = product_settings::load(&state.database, &keys).await?;
	enforce_settings(&state, &products, &resolved, &settings, player, buyer).await?;

	// Priced here, not at PayNow: the sales and coupons are ours, and the
	// buyer has to be charged the number the store showed them.
	let applied = apply_discounts(
		&state,
		&products,
		&resolved,
		&settings,
		&promo_codes,
		buyer,
	)
	.await?;

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
		(
			"quoted_discount".to_string(),
			applied.quoted_discount_minor.to_string(),
		),
	]);

	let session = state
		.paynow
		.client
		.create_checkout(NewCheckout {
			customer_id: &buyer_customer,
			lines,
			// Sales apply on their own; codes have to be passed.
			promo_codes: applied.codes,
			return_url: &state.paynow.return_url,
			cancel_url: &state.paynow.cancel_url,
			metadata,
			customer_ip: ip,
		})
		.await
		// Everything else was validated first, so PayNow blaming the request
		// means a code the buyer can correct.
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

	match state.checkout_limit.check(address_key(ip)).await {
		Some(_) => Err(CreateError::RateLimited),
		None => Ok(()),
	}
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
		// A rental can be extended or bought outright.
		.filter(player_owned_cosmetic::Column::ExpiresAt.is_null())
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

/// Stock is checked, not reserved: it is virtual, so a rare oversell by a
/// racing checkout costs nothing.
async fn enforce_settings(
	state: &ApiState,
	product_ids: &[String],
	resolved: &HashMap<String, Product>,
	settings: &HashMap<Key, Settings>,
	player: Uuid,
	buyer: Uuid,
) -> Result<(), CreateError> {
	if settings.is_empty() {
		return Ok(());
	}

	let now = Utc::now().fixed_offset();
	let sold = product_settings::sold(&state.database, settings).await?;
	let player_id = user_id(state, player).await?;
	let buyer_id = if buyer == player {
		player_id
	} else {
		user_id(state, buyer).await?
	};

	let owned: HashSet<i32> = match player_id {
		Some(id) if settings.values().any(|s| !s.requirements.is_empty()) => {
			PlayerOwnedCosmetic::find()
				.filter(player_owned_cosmetic::Column::PlayerId.eq(id))
				.all(&state.database)
				.await?
				.into_iter()
				.map(|row| row.cosmetic_id)
				.collect()
		}
		_ => HashSet::new(),
	};

	for product in product_ids.iter().filter_map(|id| resolved.get(id)) {
		let key = product.key();
		let Some(settings) = settings.get(&key) else {
			continue;
		};
		let model = &settings.model;

		if !settings.is_available(now) {
			return Err(CreateError::NotAvailable(product.name()));
		}
		if model.gifting_disabled && buyer != player {
			return Err(CreateError::GiftingDisabled(product.name()));
		}
		if !settings.requirements_met(&owned) {
			return Err(CreateError::MissingRequirement(product.name()));
		}
		if model
			.stock_limit
			.is_some_and(|limit| sold.get(&key).copied().unwrap_or(0) >= i64::from(limit))
		{
			return Err(CreateError::SoldOut(product.name()));
		}
		if let (Some(limit), Some(buyer_id)) = (model.customer_limit, buyer_id)
			&& product_settings::bought_by(&state.database, key, model, buyer_id).await?
				>= u64::try_from(limit).unwrap_or(0)
		{
			return Err(CreateError::LimitReached(product.name()));
		}
	}

	Ok(())
}

async fn user_id(state: &ApiState, player: Uuid) -> Result<Option<i32>, CreateError> {
	Ok(User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.one(&state.database)
		.await?
		.map(|user| user.id))
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

/// What a basket's discounts resolved to: the ids to record once the order
/// completes, and the codes PayNow needs to charge the same total.
struct AppliedDiscounts {
	/// As PayNow knows them.
	codes: Vec<String>,
	/// What PayNow should take off, checked against the order it reports.
	quoted_discount_minor: i64,
}

/// Quotes the basket against our own sales and coupons.
async fn apply_discounts(
	state: &ApiState,
	product_ids: &[String],
	resolved: &HashMap<String, Product>,
	settings: &HashMap<Key, Settings>,
	codes: &[String],
	buyer: Uuid,
) -> Result<AppliedDiscounts, CreateError> {
	let buyer_id = user_id(state, buyer).await?;

	let live =
		live_rules(&state.database, codes, buyer_id, Utc::now().into()).await?;

	// A code that matched nothing is the buyer's typo, or a campaign that has
	// ended. Either way they should be told, not silently charged full price.
	// One PayNow does not carry cannot be charged there either.
	let honoured: HashSet<String> = live
		.iter()
		.filter(|discount| discount.model.paynow_id.is_some())
		.filter_map(|discount| discount.model.code.clone())
		.collect();
	let mut paynow_codes = Vec::with_capacity(codes.len());
	for code in codes {
		let code = normalise_code(code);
		if !honoured.contains(&code) {
			return Err(CreateError::UnknownPromoCode(code));
		}
		paynow_codes.push(code);
	}

	let tags = tags_by_cosmetic(state, resolved).await?;
	let lines: Vec<Sellable> = product_ids
		.iter()
		.filter_map(|product_id| {
			let product = resolved.get(product_id)?;
			// A bundle carries no tags of its own on PayNow, so none here.
			let tags: Vec<i32> = match product {
				Product::Bundle { .. } => Vec::new(),
				_ => product
					.cosmetics()
					.iter()
					.filter_map(|cosmetic| tags.get(&cosmetic.id))
					.flatten()
					.copied()
					.collect(),
			};

			let coupons_disabled = settings
				.get(&product.key())
				.is_some_and(|settings| settings.model.coupons_disabled);

			Some(product.sellable(product_id, &tags, coupons_disabled))
		})
		.collect();

	let rules: Vec<Rule> = live.iter().map(|discount| discount.rule.clone()).collect();
	let quoted = quote(&lines, &rules, CHECKOUT_CURRENCY);

	Ok(AppliedDiscounts {
		codes: paynow_codes,
		quoted_discount_minor: quoted.discount_minor,
	})
}

/// Tag ids per cosmetic, for the rules that target a tag.
async fn tags_by_cosmetic(
	state: &ApiState,
	resolved: &HashMap<String, Product>,
) -> Result<HashMap<i32, Vec<i32>>, CreateError> {
	let ids: Vec<i32> = resolved
		.values()
		.flat_map(Product::cosmetics)
		.map(|cosmetic| cosmetic.id)
		.collect();
	if ids.is_empty() {
		return Ok(HashMap::new());
	}

	let mut tags: HashMap<i32, Vec<i32>> = HashMap::new();
	for row in TagsCosmetic::find()
		.filter(tags_cosmetic::Column::CosmeticId.is_in(ids))
		.all(&state.database)
		.await?
	{
		tags.entry(row.cosmetic_id).or_default().push(row.tag_id);
	}

	Ok(tags)
}

/// The store sells in one currency; `provision-paynow` refuses to run against
/// a store that does not.
const CHECKOUT_CURRENCY: &str = "usd";
