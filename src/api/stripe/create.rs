use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use stripe_checkout::CheckoutSessionMode;
use stripe_checkout::checkout_session::{
	CreateCheckoutSession, CreateCheckoutSessionLineItems,
};
use uuid::Uuid;

use crate::api::ApiState;

#[derive(Debug, thiserror::Error)]
pub(super) enum CreateError {
	#[error("Unable to create checkout session: {0}")]
	Stripe(#[from] stripe_client::StripeError),
	#[error("Stripe did not return a checkout url")]
	MissingUrl,
}

impl IntoResponse for CreateError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				CreateError::Stripe(_) => StatusCode::BAD_GATEWAY,
				CreateError::MissingUrl => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateRequest {
	/// The Minecraft UUID of the receiving player
	player: Uuid,
	/// The Minecraft UUID of the buyer, None if player == buyer
	buyer: Option<Uuid>,
	/// The Stripe price ids to charge for, one checkout line each
	prices: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct CreateResponse {
	/// The Stripe-hosted checkout page url to redirect the buyer to
	url: String,
}

pub(super) async fn endpoint(
	State(state): State<ApiState>,
	Json(request): Json<CreateRequest>,
) -> Result<Json<CreateResponse>, CreateError> {
	let CreateRequest {
		player,
		prices,
		buyer,
	} = request;

	let line_items = prices
		.iter()
		.map(|price| {
			let mut item = CreateCheckoutSessionLineItems::new();
			item.price = Some(price.clone());
			item.quantity = Some(1);
			item
		})
		.collect::<Vec<_>>();

	let metadata = HashMap::from([
		("player".to_string(), player.to_string()), // minecraft uuid!!
		(
			"buyer".to_string(),
			buyer.map_or_else(|| player.to_string(), |b| b.to_string()),
		), // minecraft uuid!!
		("prices".to_string(), prices.join(",")),
	]);

	let session = CreateCheckoutSession::new()
		.line_items(line_items)
		.mode(CheckoutSessionMode::Payment)
		.success_url(state.stripe.success_url.clone())
		.cancel_url(state.stripe.cancel_url.clone())
		.metadata(metadata)
		.send(&state.stripe.client)
		.await?;

	session
		.url
		.map(|url| Json(CreateResponse { url }))
		.ok_or(CreateError::MissingUrl)
}
