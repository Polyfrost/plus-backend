use axum::{
	body::Bytes,
	extract::State,
	http::{HeaderMap, StatusCode},
};
use chrono::Utc;
use entities::{
	paynow_webhook_event, prelude::*, sea_orm_active_enums::TransactionStatus,
	transaction, transaction_line, user,
};
use sea_orm::{
	ActiveValue, DbErr, Set, TransactionError, TransactionTrait, prelude::*,
	sea_query::OnConflict,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{
	grant::{self, GrantContext, Grants},
	refund::{Attribution, attribute},
};
use crate::{
	api::{
		ApiState,
		v0::websocket::{send_to_owner, structs::ClientBoundPacket},
	},
	database::{DatabaseTransactionExt, DatabaseUserExt, OrderCharge},
	paynow::{
		models::{Order, Payment, WebhookEnvelope},
		webhook::verify,
	},
};

/// Verifies the signature, then grants, revokes or restores the cosmetics an
/// order covers. Events we do not act on are acknowledged, not retried.
pub(super) async fn endpoint(
	State(state): State<ApiState>,
	headers: HeaderMap,
	body: Bytes,
) -> StatusCode {
	if let Err(error) = verify(
		state.paynow.webhook_secret.as_bytes(),
		&headers,
		&body,
		Utc::now().timestamp_millis(),
	) {
		warn!("Rejected PayNow webhook: {error}");
		return StatusCode::BAD_REQUEST;
	}

	let envelope: WebhookEnvelope = match serde_json::from_slice(&body) {
		Ok(envelope) => envelope,
		Err(error) => {
			warn!("Unable to parse PayNow webhook body: {error}");
			return StatusCode::BAD_REQUEST;
		}
	};

	// Decoded only once the type is known, so an unhandled event cannot fail
	// to parse.
	let outcome = match envelope.event_type.as_str() {
		"ON_ORDER_COMPLETED" => match decode(&envelope) {
			Ok(order) => handle_order(&state, &envelope.event_id, order).await,
			Err(status) => return status,
		},
		"ON_REFUND" => match decode(&envelope) {
			Ok(payment) => handle_refund(&state, &envelope.event_id, payment).await,
			Err(status) => return status,
		},
		"ON_CHARGEBACK" => match decode(&envelope) {
			Ok(payment) => handle_chargeback(&state, &envelope.event_id, payment).await,
			Err(status) => return status,
		},
		"ON_CHARGEBACK_CLOSED" => match decode(&envelope) {
			Ok(payment) => {
				handle_chargeback_closed(&state, &envelope.event_id, payment).await
			}
			Err(status) => return status,
		},
		other => {
			debug!(event = other, "Ignoring PayNow webhook");
			return StatusCode::OK;
		}
	};

	match outcome {
		Ok(Some((grants, revoked))) => {
			broadcast(&state, grants, revoked).await;
			StatusCode::OK
		}
		Ok(None) => StatusCode::OK,
		// A 500 makes PayNow retry, which is right for a database failure.
		Err(error) => {
			warn!(event = %envelope.event_type, "Unable to process PayNow webhook: {error}");
			StatusCode::INTERNAL_SERVER_ERROR
		}
	}
}

fn decode<T: serde::de::DeserializeOwned>(
	envelope: &WebhookEnvelope,
) -> Result<T, StatusCode> {
	serde_json::from_value(envelope.body.clone()).map_err(|error| {
		warn!(event = %envelope.event_type, "Unable to decode PayNow webhook body: {error}");
		StatusCode::BAD_REQUEST
	})
}

type Handled = Result<Option<(Grants, bool)>, DbErr>;

async fn handle_order(state: &ApiState, event_id: &str, order: Order) -> Handled {
	if !order.status.eq_ignore_ascii_case("completed") {
		debug!(order = %order.id, status = %order.status, "Ignoring non-completed order");
		return Ok(None);
	}

	let metadata = order
		.checkout
		.as_ref()
		.map(|checkout| checkout.metadata.clone())
		.unwrap_or_default();
	let order_customer = order.customer.as_ref().and_then(|customer| customer.uuid());

	let Some(player) = metadata
		.get("player")
		.and_then(|value| Uuid::parse_str(value).ok())
		.or(order_customer)
	else {
		warn!(order = %order.id, "Completed order has no player to grant to");
		return Ok(None);
	};
	let buyer = metadata
		.get("buyer")
		.and_then(|value| Uuid::parse_str(value).ok())
		.or(order_customer)
		.unwrap_or(player);

	let charge = OrderCharge {
		amount_minor: Some(order.total_amount),
		currency: order.currency.clone(),
		discount_minor: Some(order.discount_amount),
	};
	let currency = order.currency.clone().unwrap_or_else(|| "usd".to_string());
	let order_id = order.id.clone();
	let event_id = event_id.to_string();

	let grants = run(state, move |txn| {
		Box::pin(async move {
			if !claim_event(txn, &event_id, "ON_ORDER_COMPLETED").await? {
				return Ok(None);
			}

			let user = User::get_or_create(txn, player).await?;
			let buyer_id = if buyer == player {
				None
			} else {
				Some(User::get_or_create(txn, buyer).await?.id)
			};

			let transaction = Transaction::get_or_create_paynow(
				txn,
				user.id,
				buyer_id,
				&order_id,
				serde_json::json!({ "order_id": order_id, "checkout_metadata": metadata }),
				charge,
			)
			.await?;

			let grants = grant::grant_lines(
				txn,
				GrantContext {
					player,
					transaction: &transaction,
					currency,
				},
				&order.lines,
			)
			.await?;

			Ok(Some(grants))
		})
	})
	.await?;

	let Some(grants) = grants else {
		return Ok(None);
	};

	info!(
		"Granted PayNow order for {} player(s): {} cosmetics, {} emotes",
		grants.len(),
		grants.values().map(|g| g.cosmetic_ids.len()).sum::<usize>(),
		grants.values().map(|g| g.emote_ids.len()).sum::<usize>(),
	);

	Ok(Some((grants, false)))
}

async fn handle_refund(state: &ApiState, event_id: &str, payment: Payment) -> Handled {
	let Some(transaction) = find_transaction(state, payment.order_id.as_deref()).await?
	else {
		return Ok(None);
	};

	let outstanding = TransactionLine::find()
		.filter(transaction_line::Column::TransactionId.eq(transaction.id))
		.filter(transaction_line::Column::ReturnedAt.is_null())
		.all(&state.database)
		.await?;

	let refunded_total = payment.refunded_total().max(0);
	let newly_refunded = (refunded_total - transaction.refunded_minor).max(0);

	// Costs a round trip, so only when the amount leaves room for doubt.
	let order = if newly_refunded
		>= outstanding.iter().map(|line| line.total_minor).sum::<i64>()
	{
		None
	} else {
		match state
			.paynow
			.client
			.order(&transaction_order_id(&transaction))
			.await
		{
			Ok(order) => Some(order),
			Err(error) => {
				warn!("Unable to read the refunded order back from PayNow: {error}");
				None
			}
		}
	};

	// Stated outright when the whole payment went back.
	let fully_refunded = payment.refund_status.as_deref().is_some_and(|status| {
		["refunded", "full", "fully_refunded"]
			.iter()
			.any(|known| status.eq_ignore_ascii_case(known))
	});

	let attribution = if fully_refunded {
		Attribution::Full
	} else {
		attribute(order.as_ref(), &outstanding, newly_refunded)
	};
	let (line_ids, status) = match attribution {
		Attribution::Full => (
			outstanding.iter().map(|line| line.id).collect::<Vec<_>>(),
			TransactionStatus::Refunded,
		),
		Attribution::Lines(line_ids) => (line_ids, TransactionStatus::Refunded),
		Attribution::Undecidable => {
			warn!(
				order = %transaction_order_id(&transaction),
				amount = newly_refunded,
				"Partial refund could not be attributed to order lines; ownership left intact"
			);
			(Vec::new(), TransactionStatus::PartiallyRefunded)
		}
	};

	let refunded_at = payment.refunded_at.unwrap_or_else(Utc::now);
	let event_id = event_id.to_string();
	let transaction_id = transaction.id;
	let buyer_id = transaction.buyer.unwrap_or(transaction.player_id);

	let grants = run(state, move |txn| {
		Box::pin(async move {
			if !claim_event(txn, &event_id, "ON_REFUND").await? {
				return Ok(None);
			}

			let grants = grant::revoke_lines(
				txn,
				transaction_id,
				&line_ids,
				refunded_at,
				status.clone(),
			)
			.await?;

			let remaining = TransactionLine::find()
				.filter(transaction_line::Column::TransactionId.eq(transaction_id))
				.filter(transaction_line::Column::ReturnedAt.is_null())
				.count(txn)
				.await?;

			let transaction_status =
				if remaining == 0 && status == TransactionStatus::Refunded {
					TransactionStatus::Refunded
				} else {
					TransactionStatus::PartiallyRefunded
				};

			finish_refund(
				txn,
				transaction_id,
				transaction_status,
				refunded_total,
				refunded_at,
			)
			.await?;
			bump_counter(txn, buyer_id, user::Column::RefundCount, 1).await?;

			Ok(Some(grants))
		})
	})
	.await?;

	Ok(grants.map(|grants| (grants, true)))
}

async fn handle_chargeback(
	state: &ApiState,
	event_id: &str,
	payment: Payment,
) -> Handled {
	let Some(transaction) = find_transaction(state, payment.order_id.as_deref()).await?
	else {
		return Ok(None);
	};

	let charged_back_at = payment.chargeback_at.unwrap_or_else(Utc::now);
	let event_id = event_id.to_string();
	let transaction_id = transaction.id;
	let buyer_id = transaction.buyer.unwrap_or(transaction.player_id);

	let grants = run(state, move |txn| {
		Box::pin(async move {
			if !claim_event(txn, &event_id, "ON_CHARGEBACK").await? {
				return Ok(None);
			}

			let line_ids = grant::outstanding_line_ids(txn, transaction_id).await?;
			let grants = grant::revoke_lines(
				txn,
				transaction_id,
				&line_ids,
				charged_back_at,
				TransactionStatus::Chargeback,
			)
			.await?;

			let mut update = transaction::ActiveModel {
				id: ActiveValue::Unchanged(transaction_id),
				..Default::default()
			};
			update.status = Set(TransactionStatus::Chargeback);
			update.charged_back_at = Set(Some(charged_back_at.fixed_offset()));
			update.update(txn).await?;

			// A chargeback says something different about a buyer.
			bump_counter(txn, buyer_id, user::Column::ChargebackCount, 1).await?;

			Ok(Some(grants))
		})
	})
	.await?;

	Ok(grants.map(|grants| (grants, true)))
}

async fn handle_chargeback_closed(
	state: &ApiState,
	event_id: &str,
	payment: Payment,
) -> Handled {
	if !payment
		.chargeback_status
		.as_deref()
		.is_some_and(|status| status.eq_ignore_ascii_case("won"))
	{
		debug!(
			status = ?payment.chargeback_status,
			"Chargeback closed without being won; nothing to restore"
		);
		return Ok(None);
	}

	let Some(transaction) = find_transaction(state, payment.order_id.as_deref()).await?
	else {
		return Ok(None);
	};

	let event_id = event_id.to_string();
	let buyer_id = transaction.buyer.unwrap_or(transaction.player_id);
	let restored_status = if transaction.refunded_minor > 0 {
		TransactionStatus::PartiallyRefunded
	} else {
		TransactionStatus::Completed
	};

	let grants = run(state, move |txn| {
		Box::pin(async move {
			if !claim_event(txn, &event_id, "ON_CHARGEBACK_CLOSED").await? {
				return Ok(None);
			}

			let grants = grant::restore_transaction(txn, &transaction).await?;

			let mut update: transaction::ActiveModel = transaction.into();
			update.status = Set(restored_status);
			update.charged_back_at = Set(None);
			update.update(txn).await?;

			bump_counter(txn, buyer_id, user::Column::ChargebackCount, -1).await?;

			Ok(Some(grants))
		})
	})
	.await?;

	Ok(grants.map(|grants| (grants, false)))
}

/// False when already handled. Kept inside the caller's transaction so a
/// failed grant rolls the claim back and PayNow's retry is not swallowed.
async fn claim_event(
	txn: &impl ConnectionTrait,
	event_id: &str,
	event_type: &str,
) -> Result<bool, DbErr> {
	let inserted = PaynowWebhookEvent::insert(paynow_webhook_event::ActiveModel {
		event_id: Set(event_id.to_string()),
		event_type: Set(event_type.to_string()),
		received_at: ActiveValue::NotSet,
	})
	.on_conflict(
		OnConflict::column(paynow_webhook_event::Column::EventId)
			.do_nothing()
			.to_owned(),
	)
	.exec_without_returning(txn)
	.await?;

	if inserted == 0 {
		debug!(event_id, "Ignoring redelivered PayNow webhook");
	}

	Ok(inserted > 0)
}

async fn find_transaction(
	state: &ApiState,
	order_id: Option<&str>,
) -> Result<Option<transaction::Model>, DbErr> {
	let Some(order_id) = order_id else {
		warn!("PayNow payment event has no order id");
		return Ok(None);
	};

	let found = Transaction::find()
		.filter(transaction::Column::ProviderTransactionId.eq(order_id))
		.one(&state.database)
		.await?;

	if found.is_none() {
		debug!(
			order = order_id,
			"PayNow payment event is for an unknown order"
		);
	}

	Ok(found)
}

fn transaction_order_id(transaction: &transaction::Model) -> String {
	transaction
		.provider_transaction_id
		.clone()
		.unwrap_or_default()
}

async fn finish_refund(
	txn: &impl ConnectionTrait,
	transaction_id: i32,
	status: TransactionStatus,
	refunded_minor: i64,
	refunded_at: chrono::DateTime<Utc>,
) -> Result<(), DbErr> {
	let mut update = transaction::ActiveModel {
		id: ActiveValue::Unchanged(transaction_id),
		..Default::default()
	};
	update.status = Set(status);
	update.refunded_minor = Set(refunded_minor);
	update.refunded_at = Set(Some(refunded_at.fixed_offset()));
	update.update(txn).await?;

	Ok(())
}

async fn bump_counter(
	txn: &impl ConnectionTrait,
	user_id: i32,
	column: user::Column,
	delta: i32,
) -> Result<(), DbErr> {
	let expression = if delta >= 0 {
		Expr::col(column).add(delta)
	} else {
		Expr::cust_with_expr("GREATEST($1 - 1, 0)", Expr::col(column))
	};

	User::update_many()
		.col_expr(column, expression)
		.filter(user::Column::Id.eq(user_id))
		.exec(txn)
		.await?;

	Ok(())
}

/// Runs `body` in one database transaction, flattening the nested error.
async fn run<F, T>(state: &ApiState, body: F) -> Result<Option<T>, DbErr>
where
	F: for<'c> FnOnce(
			&'c sea_orm::DatabaseTransaction,
		) -> std::pin::Pin<
			Box<dyn std::future::Future<Output = Result<Option<T>, DbErr>> + Send + 'c>,
		> + Send,
	T: Send,
{
	state
		.database
		.transaction::<_, Option<T>, DbErr>(body)
		.await
		.map_err(|error| match error {
			TransactionError::Connection(error) => error,
			TransactionError::Transaction(error) => error,
		})
}

async fn broadcast(state: &ApiState, grants: Grants, revoked: bool) {
	for (player, grant) in grants {
		send_to_owner(state, player, || ClientBoundPacket::OwnershipUpdated {
			player,
			cosmetic_ids: grant.cosmetic_ids.clone(),
			emote_ids: grant.emote_ids.clone(),
			revoked,
		})
		.await;
	}
}
