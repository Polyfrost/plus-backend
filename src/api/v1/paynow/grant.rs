use std::collections::{HashMap, HashSet};

use chrono::Utc;
use entities::{
	cosmetic, ownership_grant,
	prelude::*,
	sea_orm_active_enums::{
		CosmeticType, OwnershipEventKind, TransactionProvider, TransactionStatus,
	},
	transaction, transaction_line, user,
};
use sea_orm::{
	DbErr, QuerySelect, Set, prelude::*, sea_query::OnConflict,
};
use tracing::warn;
use uuid::Uuid;

use super::resolve::{Product, resolve_products};
use crate::{
	database::DatabaseUserExt,
	ownership::{self, Settled},
	paynow::models::OrderLine,
	product_settings,
};

/// What changed for one player, ready to push over the websocket.
#[derive(Debug, Default)]
pub(super) struct OwnershipGrant {
	pub cosmetic_ids: Vec<i32>,
	pub emote_ids: Vec<i32>,
}

impl OwnershipGrant {
	pub(super) fn push(&mut self, cosmetic: &cosmetic::Model) {
		if matches!(cosmetic.r#type, CosmeticType::Emote) {
			self.emote_ids.push(cosmetic.id);
		} else {
			self.cosmetic_ids.push(cosmetic.id);
		}
	}

	fn is_empty(&self) -> bool {
		self.cosmetic_ids.is_empty() && self.emote_ids.is_empty()
	}
}

/// Keyed by recipient: one order can gift separate lines to separate players.
pub(super) type Grants = HashMap<Uuid, OwnershipGrant>;

pub(super) struct GrantContext<'a> {
	pub player: Uuid,
	pub transaction: &'a transaction::Model,
	pub currency: String,
}

/// Lines already recorded are skipped, so a redelivery is a no-op.
pub(super) async fn grant_lines(
	txn: &impl ConnectionTrait,
	context: GrantContext<'_>,
	lines: &[OrderLine],
) -> Result<Grants, DbErr> {
	let mut grants = Grants::new();

	for line in lines {
		let recipient_uuid = line
			.gift_to_customer
			.as_ref()
			.and_then(|customer| customer.uuid())
			.unwrap_or(context.player);
		let recipient = User::get_or_create(txn, recipient_uuid).await?;

		let product =
			resolve_products(txn, std::slice::from_ref(&line.product_id), false)
				.await?
				.remove(&line.product_id);

		let Some(stored) =
			insert_line(txn, &context, line, &product, recipient.id).await?
		else {
			// Already recorded by an earlier delivery of this order.
			continue;
		};

		let Some(product) = product else {
			warn!(
				product = %line.product_id,
				order = %context.transaction.provider_transaction_id.as_deref().unwrap_or_default(),
				"Paid order line does not match any cosmetic or bundle"
			);
			continue;
		};

		// Not filtered by `enabled`: a cosmetic disabled between checkout and
		// payment has still been paid for.
		let cosmetics = product.cosmetics();
		if cosmetics.is_empty() {
			continue;
		}

		let rental_days = product_settings::load(txn, &[product.key()])
			.await?
			.remove(&product.key())
			.and_then(|settings| settings.model.expires_after_days);
		let cosmetic_ids: Vec<i32> = cosmetics.iter().map(|cosmetic| cosmetic.id).collect();
		ownership::record(
			txn,
			recipient.id,
			&cosmetic_ids,
			TransactionProvider::Paynow,
			Some(context.transaction.id),
			Some(stored.id),
			rental_days,
		)
		.await?;

		// New or extended. A copy already owned for good is left as it was.
		let granted: Vec<(i32, Option<DateTimeWithTimeZone>)> =
			ownership::settle(txn, recipient.id, &cosmetic_ids)
				.await?
				.into_iter()
				.filter_map(|(id, settled)| match settled {
					Settled::Added(held) | Settled::Changed(held) => Some((id, held.expires_at())),
					Settled::Removed | Settled::Unchanged => None,
				})
				.collect();
		if granted.is_empty() {
			continue;
		}
		let granted_ids: Vec<i32> = granted.iter().map(|(id, _)| *id).collect();

		bump_purchase_count(txn, &granted_ids, 1).await?;
		for (expires_at, cosmetic_ids) in by_expiry(&granted) {
			crate::database::record_ownership_events(
				txn,
				recipient.id,
				&cosmetic_ids,
				OwnershipEventKind::Granted,
				TransactionProvider::Paynow,
				Some(context.transaction.id),
				Some(stored.id),
				expires_at,
			)
			.await?;
		}

		let grant = grants.entry(recipient_uuid).or_default();
		for cosmetic in cosmetics {
			if granted_ids.contains(&cosmetic.id) {
				grant.push(cosmetic);
			}
		}
	}

	grants.retain(|_, grant| !grant.is_empty());
	Ok(grants)
}

/// Events carry the expiry, so one call per distinct expiry.
fn by_expiry(
	rows: &[(i32, Option<DateTimeWithTimeZone>)],
) -> HashMap<Option<DateTimeWithTimeZone>, Vec<i32>> {
	let mut grouped: HashMap<Option<DateTimeWithTimeZone>, Vec<i32>> = HashMap::new();
	for (cosmetic_id, expires_at) in rows {
		grouped.entry(*expires_at).or_default().push(*cosmetic_id);
	}
	grouped
}

/// Inserts the line, returning `None` when it was already recorded.
async fn insert_line(
	txn: &impl ConnectionTrait,
	context: &GrantContext<'_>,
	line: &OrderLine,
	product: &Option<Product>,
	recipient_id: i32,
) -> Result<Option<transaction_line::Model>, DbErr> {
	let (bundle_id, group_id, cosmetic_id) = match product {
		Some(Product::Bundle { bundle, .. }) => (Some(bundle.id), None, None),
		Some(Product::CosmeticGroup { group_id, .. }) => (None, Some(*group_id), None),
		Some(Product::Cosmetic(cosmetic)) => (None, None, Some(cosmetic.id)),
		None => (None, None, None),
	};

	let inserted = TransactionLine::insert(transaction_line::ActiveModel {
		transaction_id: Set(context.transaction.id),
		provider_line_id: Set(line.id.clone()),
		product_id: Set(line.product_id.clone()),
		bundle_id: Set(bundle_id),
		cosmetic_group_id: Set(group_id),
		cosmetic_id: Set(cosmetic_id),
		recipient_id: Set(Some(recipient_id)),
		quantity: Set(line.quantity.max(1)),
		price_minor: Set(line.price),
		discount_minor: Set(line.discount_amount),
		subtotal_minor: Set(line.subtotal_amount),
		tax_minor: Set(line.tax_amount),
		total_minor: Set(line.total_amount),
		currency: Set(context.currency.clone()),
		status: Set(TransactionStatus::Completed),
		..Default::default()
	})
	.on_conflict(
		OnConflict::column(transaction_line::Column::ProviderLineId)
			.do_nothing()
			.to_owned(),
	)
	.exec_without_returning(txn)
	.await?;

	if inserted == 0 {
		return Ok(None);
	}

	TransactionLine::find()
		.filter(transaction_line::Column::ProviderLineId.eq(line.id.clone()))
		.one(txn)
		.await
}

/// Takes back what the given lines granted and marks them returned. Anything
/// the player also holds through another purchase stays, for whatever time
/// that purchase leaves.
pub(super) async fn revoke_lines(
	txn: &impl ConnectionTrait,
	line_ids: &[i64],
	returned_at: chrono::DateTime<Utc>,
	status: TransactionStatus,
) -> Result<Grants, DbErr> {
	let revoked = ownership::set_lines_active(txn, line_ids, false).await?;
	let grants = settle_moved(txn, &revoked, OwnershipEventKind::Revoked).await?;
	mark_lines(txn, line_ids, returned_at, status).await?;
	Ok(grants)
}

/// Settles every player whose grants just moved, with one event per grant.
async fn settle_moved(
	txn: &impl ConnectionTrait,
	moved: &[ownership_grant::Model],
	kind: OwnershipEventKind,
) -> Result<Grants, DbErr> {
	let restoring = matches!(kind, OwnershipEventKind::Granted);
	let mut by_player: HashMap<i32, Vec<&ownership_grant::Model>> = HashMap::new();
	for grant in moved {
		by_player.entry(grant.player_id).or_default().push(grant);
	}
	if by_player.is_empty() {
		return Ok(Grants::new());
	}

	let cosmetics =
		cosmetics_by_id(txn, moved.iter().map(|grant| grant.cosmetic_id).collect()).await?;
	let uuids = uuids_by_id(txn, by_player.keys().copied().collect()).await?;

	let mut grants = Grants::new();
	for (player_id, moved) in by_player {
		let ids: Vec<i32> = moved.iter().map(|grant| grant.cosmetic_id).collect();
		let settled = ownership::settle(txn, player_id, &ids).await?;

		bump_purchase_count(txn, &ids, if restoring { 1 } else { -1 }).await?;
		for grant in &moved {
			let expires_at = match settled.get(&grant.cosmetic_id) {
				Some(Settled::Added(held) | Settled::Changed(held)) => held.expires_at(),
				_ => None,
			};
			crate::database::record_ownership_events(
				txn,
				player_id,
				&[grant.cosmetic_id],
				kind.clone(),
				grant.provider.clone(),
				grant.transaction_id,
				grant.transaction_line_id,
				expires_at,
			)
			.await?;
		}

		let Some(uuid) = uuids.get(&player_id) else {
			continue;
		};
		let grant = grants.entry(*uuid).or_default();
		for (cosmetic_id, settled) in &settled {
			let notify = match settled {
				Settled::Removed => !restoring,
				Settled::Added(_) | Settled::Changed(_) => restoring,
				Settled::Unchanged => false,
			};
			if notify && let Some(cosmetic) = cosmetics.get(cosmetic_id) {
				grant.push(cosmetic);
			}
		}
	}

	grants.retain(|_, grant| !grant.is_empty());
	Ok(grants)
}

async fn mark_lines(
	txn: &impl ConnectionTrait,
	line_ids: &[i64],
	returned_at: chrono::DateTime<Utc>,
	status: TransactionStatus,
) -> Result<(), DbErr> {
	TransactionLine::update_many()
		.col_expr(transaction_line::Column::Status, status.as_enum())
		.col_expr(
			transaction_line::Column::ReturnedAt,
			Expr::value(returned_at.fixed_offset()),
		)
		.col_expr(
			transaction_line::Column::ReturnedMinor,
			Expr::col(transaction_line::Column::TotalMinor).into(),
		)
		.filter(transaction_line::Column::Id.is_in(line_ids.to_vec()))
		.exec(txn)
		.await?;

	Ok(())
}

/// Turns the disputed lines' grants back on. A rental whose time ran out
/// during the dispute stays gone.
pub(super) async fn restore_transaction(
	txn: &impl ConnectionTrait,
	transaction: &transaction::Model,
) -> Result<Grants, DbErr> {
	// A line refunded before the dispute was legitimately returned.
	let disputed: Vec<i64> = TransactionLine::find()
		.filter(transaction_line::Column::TransactionId.eq(transaction.id))
		.filter(transaction_line::Column::Status.eq(TransactionStatus::Chargeback))
		.all(txn)
		.await?
		.into_iter()
		.map(|line| line.id)
		.collect();
	if disputed.is_empty() {
		return Ok(Grants::new());
	}

	let restored = ownership::set_lines_active(txn, &disputed, true).await?;
	let grants = settle_moved(txn, &restored, OwnershipEventKind::Granted).await?;

	TransactionLine::update_many()
		.col_expr(
			transaction_line::Column::Status,
			TransactionStatus::Completed.as_enum(),
		)
		.col_expr(transaction_line::Column::ReturnedMinor, Expr::value(0i64))
		.col_expr(
			transaction_line::Column::ReturnedAt,
			Expr::value(Option::<chrono::DateTime<chrono::FixedOffset>>::None),
		)
		.filter(transaction_line::Column::Id.is_in(disputed))
		.exec(txn)
		.await?;

	Ok(grants)
}

/// A chargeback after a partial refund only disputes what is left.
pub(super) async fn outstanding_line_ids(
	txn: &impl ConnectionTrait,
	transaction_id: i32,
) -> Result<Vec<i64>, DbErr> {
	Ok(TransactionLine::find()
		.filter(transaction_line::Column::TransactionId.eq(transaction_id))
		.filter(transaction_line::Column::ReturnedAt.is_null())
		.all(txn)
		.await?
		.into_iter()
		.map(|line| line.id)
		.collect())
}

async fn bump_purchase_count(
	txn: &impl ConnectionTrait,
	cosmetic_ids: &[i32],
	delta: i32,
) -> Result<(), DbErr> {
	if cosmetic_ids.is_empty() {
		return Ok(());
	}

	let expression = if delta >= 0 {
		Expr::col(cosmetic::Column::PurchaseCount).add(delta)
	} else {
		Expr::cust_with_exprs(
			"GREATEST($1 + $2, 0)",
			[
				Expr::col(cosmetic::Column::PurchaseCount).into(),
				Expr::value(delta),
			],
		)
	};

	Cosmetic::update_many()
		.col_expr(cosmetic::Column::PurchaseCount, expression)
		.filter(cosmetic::Column::Id.is_in(cosmetic_ids.to_vec()))
		.exec(txn)
		.await?;

	Ok(())
}

pub(super) async fn cosmetics_by_id(
	txn: &impl ConnectionTrait,
	ids: Vec<i32>,
) -> Result<HashMap<i32, cosmetic::Model>, DbErr> {
	let unique: HashSet<i32> = ids.into_iter().collect();
	Ok(Cosmetic::find()
		.filter(cosmetic::Column::Id.is_in(unique))
		.all(txn)
		.await?
		.into_iter()
		.map(|cosmetic| (cosmetic.id, cosmetic))
		.collect())
}

pub(super) async fn uuids_by_id(
	txn: &impl ConnectionTrait,
	ids: Vec<i32>,
) -> Result<HashMap<i32, Uuid>, DbErr> {
	let unique: HashSet<i32> = ids.into_iter().collect();
	Ok(User::find()
		.filter(user::Column::Id.is_in(unique))
		.select_only()
		.column(user::Column::Id)
		.column(user::Column::MinecraftUuid)
		.into_tuple::<(i32, Uuid)>()
		.all(txn)
		.await?
		.into_iter()
		.collect())
}
