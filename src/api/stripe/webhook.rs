use axum::{
	body::Bytes,
	extract::State,
	http::{HeaderMap, StatusCode},
};
use entities::{
	cosmetic, emote, player_owned_cosmetic, player_owned_emote,
	prelude::*,
	sea_orm_active_enums::{TransactionProvider, TransactionStatus},
	transaction,
};
use sea_orm::{ActiveValue, DbErr, TransactionError, TransactionTrait, prelude::*};
use stripe_checkout::checkout_session::ListCheckoutSession;
use stripe_shared::{Charge, CheckoutSessionPaymentStatus};
use stripe_webhook::{EventObject, Webhook};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
	api::{ApiState, websocket::structs::ClientBoundPacket},
	database::{DatabaseTransactionExt, DatabaseUserExt},
};

#[derive(Debug, Default)]
struct OwnershipGrant {
	cosmetic_ids: Vec<i32>,
	emote_ids: Vec<i32>,
}

/// Stripe webhook endpoint. Verifies the signature and grants the purchased
/// cosmetics/emotes for paid checkout sessions, revoking them again on a full
/// refund; all other events are a no-op.
pub(super) async fn endpoint(
	State(state): State<ApiState>,
	headers: HeaderMap,
	body: Bytes,
) -> StatusCode {
	let Some(signature) = headers
		.get("stripe-signature")
		.and_then(|value| value.to_str().ok())
	else {
		return StatusCode::BAD_REQUEST;
	};

	let Ok(payload) = str::from_utf8(&body) else {
		return StatusCode::BAD_REQUEST;
	};

	let event = match Webhook::construct_event(
		payload,
		signature,
		&state.stripe.webhook_secret,
	) {
		Ok(event) => event,
		Err(error) => {
			warn!("Rejected Stripe webhook: {error}");
			return StatusCode::BAD_REQUEST;
		}
	};

	if let EventObject::ChargeRefunded(charge) = event.data.object {
		return handle_refund(&state, *charge).await;
	}

	// async payments are bank transfers idk if you're supporting that but hey
	let (EventObject::CheckoutSessionCompleted(session)
	| EventObject::CheckoutSessionAsyncPaymentSucceeded(session)) = event.data.object
	else {
		return StatusCode::OK;
	};

	// paid or free items
	if session.payment_status != CheckoutSessionPaymentStatus::Paid
		&& session.payment_status != CheckoutSessionPaymentStatus::NoPaymentRequired
	{
		return StatusCode::OK;
	}

	let metadata = session.metadata.unwrap_or_default();
	let Some(player) = metadata.get("player").and_then(|p| Uuid::parse_str(p).ok())
	else {
		warn!(
			"Paid checkout session {:?} missing valid player metadata",
			session.id
		);
		return StatusCode::BAD_REQUEST;
	};
	let Some(buyer) = metadata.get("buyer").and_then(|p| Uuid::parse_str(p).ok()) else {
		warn!(
			"Paid checkout session {:?} missing valid buyer metadata",
			session.id
		);
		return StatusCode::BAD_REQUEST;
	};

	let prices: Vec<String> = metadata
		.get("prices")
		.map(|p| {
			p.split(',')
				.filter(|s| !s.is_empty())
				.map(str::to_string)
				.collect()
		})
		.unwrap_or_default();
	let session_id = session.id.to_string();

	let grant = state
		.database
		.transaction::<_, OwnershipGrant, DbErr>(|txn| {
			Box::pin(async move {
				let user = User::get_or_create(txn, player).await?;
				let buyer_id = if buyer != player {
					Some(User::get_or_create(txn, buyer).await?.id)
				} else {
					None
				};
				let transaction = Transaction::get_or_create_stripe(
					txn,
					user.id,
					buyer_id,
					&session_id,
					serde_json::json!({ "session_id": session_id.clone() }),
				)
				.await?;

				let mut grant = OwnershipGrant::default();
				for price in &prices {
					let cosmetics = Cosmetic::find()
						.filter(cosmetic::Column::StripePriceId.eq(price.as_str()))
						.all(txn)
						.await?;
					if !cosmetics.is_empty() {
						PlayerOwnedCosmetic::insert_many(cosmetics.iter().map(
							|cosmetic| player_owned_cosmetic::ActiveModel {
								player_id: ActiveValue::Set(user.id),
								cosmetic_id: ActiveValue::Set(cosmetic.id),
								acquired_via: ActiveValue::Set(
									TransactionProvider::Stripe,
								),
								transaction_id: ActiveValue::Set(Some(transaction.id)),
								..Default::default()
							},
						))
						.on_conflict_do_nothing()
						.exec(txn)
						.await?;
						grant
							.cosmetic_ids
							.extend(cosmetics.into_iter().map(|cosmetic| cosmetic.id));
					}

					let emotes = Emote::find()
						.filter(emote::Column::StripePriceId.eq(price.as_str()))
						.all(txn)
						.await?;
					if !emotes.is_empty() {
						PlayerOwnedEmote::insert_many(emotes.iter().map(|emote| {
							player_owned_emote::ActiveModel {
								player_id: ActiveValue::Set(user.id),
								emote_id: ActiveValue::Set(emote.id),
								acquired_via: ActiveValue::Set(
									TransactionProvider::Stripe,
								),
								transaction_id: ActiveValue::Set(Some(transaction.id)),
								..Default::default()
							}
						}))
						.on_conflict_do_nothing()
						.exec(txn)
						.await?;
						grant
							.emote_ids
							.extend(emotes.into_iter().map(|emote| emote.id));
					}
				}

				Ok(grant)
			})
		})
		.await;

	let grant = match grant {
		Ok(grant) => grant,
		Err(error) => {
			let error = match error {
				TransactionError::Connection(error) => error,
				TransactionError::Transaction(error) => error,
			};
			warn!("Failed to grant stripe purchase: {error}");
			return StatusCode::INTERNAL_SERVER_ERROR;
		}
	};

	info!(
		"Granted stripe purchase for player {player}: {} cosmetics, {} emotes",
		grant.cosmetic_ids.len(),
		grant.emote_ids.len()
	);

	if !grant.cosmetic_ids.is_empty() || !grant.emote_ids.is_empty() {
		let connection_ids = state
			.realtime
			.connections_by_owner
			.read()
			.await
			.get(&player)
			.cloned()
			.unwrap_or_default();
		if !connection_ids.is_empty() {
			let connections = state.realtime.connections.read().await;
			for connection_id in connection_ids {
				let Some(connection) = connections.get(&connection_id) else {
					continue;
				};
				let _ = connection.tx.send(ClientBoundPacket::OwnershipUpdated {
					player,
					cosmetic_ids: grant.cosmetic_ids.clone(),
					emote_ids: grant.emote_ids.clone(),
				});
			}
		}
	}

	StatusCode::OK
}

/// Revokes the cosmetics/emotes granted by a fully refunded charge and marks
/// the backing transaction refunded. Partial refunds are left untouched.
async fn handle_refund(state: &ApiState, charge: Charge) -> StatusCode {
	// partial refunds don't have a binary answer so it needs to be manual
	// refunds should be uncommon tho?
	if !charge.refunded {
		return StatusCode::OK;
	}

	let Some(payment_intent) = charge.payment_intent else {
		warn!("Refunded charge {:?} has no payment intent", charge.id);
		return StatusCode::BAD_REQUEST;
	};
	let payment_intent = payment_intent.id().to_string();

	let session = match ListCheckoutSession::new()
		.payment_intent(payment_intent.clone())
		.send(&state.stripe.client)
		.await
	{
		Ok(list) => list.data.into_iter().next(),
		Err(error) => {
			warn!("Failed to look up checkout session for refund: {error}");
			return StatusCode::BAD_GATEWAY;
		}
	};
	let Some(session) = session else {
		warn!("No checkout session for refunded payment intent {payment_intent}");
		return StatusCode::OK;
	};

	let metadata = session.metadata.unwrap_or_default();
	let Some(player) = metadata.get("player").and_then(|p| Uuid::parse_str(p).ok())
	else {
		warn!(
			"Refunded checkout session {:?} missing valid player metadata",
			session.id
		);
		return StatusCode::BAD_REQUEST;
	};
	let Some(buyer) = metadata.get("buyer").and_then(|p| Uuid::parse_str(p).ok()) else {
		warn!(
			"Refunded checkout session {:?} missing valid buyer metadata",
			session.id
		);
		return StatusCode::BAD_REQUEST;
	};

	let session_id = session.id.to_string();

	let revoked = state
		.database
		.transaction::<_, (u64, u64), DbErr>(|txn| {
			Box::pin(async move {
				let user = User::get_or_create(txn, player).await?;
				let buyer_id = if buyer != player {
					Some(User::get_or_create(txn, buyer).await?.id)
				} else {
					None
				};
				let transaction = Transaction::get_or_create_stripe(
					txn,
					user.id,
					buyer_id,
					&session_id,
					serde_json::json!({ "session_id": session_id.clone() }),
				)
				.await?;

				// Revoke every cosmetic and emote this transaction granted.
				let cosmetics = PlayerOwnedCosmetic::delete_many()
					.filter(
						player_owned_cosmetic::Column::TransactionId.eq(transaction.id),
					)
					.exec(txn)
					.await?;
				let emotes = PlayerOwnedEmote::delete_many()
					.filter(player_owned_emote::Column::TransactionId.eq(transaction.id))
					.exec(txn)
					.await?;

				let mut transaction: transaction::ActiveModel = transaction.into();
				transaction.status = ActiveValue::Set(TransactionStatus::Refunded);
				transaction.update(txn).await?;

				Ok((cosmetics.rows_affected, emotes.rows_affected))
			})
		})
		.await;

	let (cosmetics, emotes) = match revoked {
		Ok(counts) => counts,
		Err(error) => {
			let error = match error {
				TransactionError::Connection(error) => error,
				TransactionError::Transaction(error) => error,
			};
			warn!("Failed to refund stripe purchase: {error}");
			return StatusCode::INTERNAL_SERVER_ERROR;
		}
	};

	info!(
		"Refunded stripe purchase for player {player}: {cosmetics} cosmetics, \
		 {emotes} emotes revoked"
	);

	StatusCode::OK
}
