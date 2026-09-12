use std::collections::{HashMap, HashSet};

use chrono::Utc;
use entities::{
	cosmetic, cosmetic_ownership_event, player_owned_cosmetic,
	prelude::*,
	sea_orm_active_enums::{
		CosmeticType, OwnershipEventKind, TransactionProvider, TransactionStatus,
	},
	transaction, transaction_line, user,
};
use sea_orm::{ActiveValue, DbErr, QuerySelect, Set, prelude::*, sea_query::OnConflict};
use tracing::warn;
use uuid::Uuid;

use super::resolve::{Product, resolve_products};
use crate::{database::DatabaseUserExt, paynow::models::OrderLine};

/// What changed for one player, ready to push over the websocket.
#[derive(Debug, Default)]
pub(super) struct OwnershipGrant {
	pub cosmetic_ids: Vec<i32>,
	pub emote_ids: Vec<i32>,
}

impl OwnershipGrant {
	fn push(&mut self, cosmetic: &cosmetic::Model) {
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

		let granted =
			PlayerOwnedCosmetic::insert_many(cosmetics.iter().map(|cosmetic| {
				player_owned_cosmetic::ActiveModel {
					player_id: Set(recipient.id),
					cosmetic_id: Set(cosmetic.id),
					acquired_via: Set(TransactionProvider::Paynow),
					transaction_id: Set(Some(context.transaction.id)),
					transaction_line_id: Set(Some(stored.id)),
					acquired_at: ActiveValue::NotSet,
				}
			}))
			.on_conflict(
				OnConflict::columns([
					player_owned_cosmetic::Column::PlayerId,
					player_owned_cosmetic::Column::CosmeticId,
				])
				.do_nothing()
				.to_owned(),
			)
			.exec_without_returning(txn)
			.await?;

		if granted == 0 {
			continue;
		}

		// The insert reports how many rows landed but not which.
		let granted_ids = owned_from_line(txn, stored.id).await?;
		if granted_ids.is_empty() {
			continue;
		}

		bump_purchase_count(txn, &granted_ids, 1).await?;
		crate::database::record_ownership_events(
			txn,
			recipient.id,
			&granted_ids,
			OwnershipEventKind::Granted,
			TransactionProvider::Paynow,
			Some(context.transaction.id),
			Some(stored.id),
		)
		.await?;

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

/// Inserts the line, returning `None` when it was already recorded.
async fn insert_line(
	txn: &impl ConnectionTrait,
	context: &GrantContext<'_>,
	line: &OrderLine,
	product: &Option<Product>,
	recipient_id: i32,
) -> Result<Option<transaction_line::Model>, DbErr> {
	let (bundle_id, group_id, cosmetic_id) = match product {
		Some(Product::Bundle { bundle_id, .. }) => (Some(*bundle_id), None, None),
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

/// Revokes everything the given lines granted and marks them returned.
pub(super) async fn revoke_lines(
	txn: &impl ConnectionTrait,
	transaction_id: i32,
	line_ids: &[i64],
	returned_at: chrono::DateTime<Utc>,
	status: TransactionStatus,
) -> Result<Grants, DbErr> {
	if line_ids.is_empty() {
		return Ok(Grants::new());
	}

	let owned = PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::TransactionLineId.is_in(line_ids.to_vec()))
		.all(txn)
		.await?;
	if owned.is_empty() {
		return mark_lines(txn, line_ids, returned_at, status)
			.await
			.map(|()| Grants::new());
	}

	let cosmetics = cosmetics_by_id(
		txn,
		owned.iter().map(|row| row.cosmetic_id).collect::<Vec<_>>(),
	)
	.await?;
	let uuids = uuids_by_id(
		txn,
		owned.iter().map(|row| row.player_id).collect::<Vec<_>>(),
	)
	.await?;

	// Grouped by line as well as player: once the owned rows are gone these
	// events are all a won chargeback has to restore from.
	let mut by_line: HashMap<(i32, Option<i64>), Vec<i32>> = HashMap::new();
	for row in &owned {
		by_line
			.entry((row.player_id, row.transaction_line_id))
			.or_default()
			.push(row.cosmetic_id);
	}

	let mut grants = Grants::new();
	for ((player_id, line_id), cosmetic_ids) in &by_line {
		// Per group, not over the whole set: two players can have held the
		// same cosmetic through two lines of one order.
		bump_purchase_count(txn, cosmetic_ids, -1).await?;
		crate::database::record_ownership_events(
			txn,
			*player_id,
			cosmetic_ids,
			OwnershipEventKind::Revoked,
			TransactionProvider::Paynow,
			Some(transaction_id),
			*line_id,
		)
		.await?;

		let Some(uuid) = uuids.get(player_id) else {
			continue;
		};
		let grant = grants.entry(*uuid).or_default();
		for cosmetic_id in cosmetic_ids {
			if let Some(cosmetic) = cosmetics.get(cosmetic_id) {
				grant.push(cosmetic);
			}
		}
	}

	PlayerOwnedCosmetic::delete_many()
		.filter(player_owned_cosmetic::Column::TransactionLineId.is_in(line_ids.to_vec()))
		.exec(txn)
		.await?;

	mark_lines(txn, line_ids, returned_at, status).await?;

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

/// Reads the ownership event trail, since the owned rows were deleted.
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

	let events = CosmeticOwnershipEvent::find()
		.filter(
			cosmetic_ownership_event::Column::TransactionLineId.is_in(disputed.clone()),
		)
		.all(txn)
		.await?;

	// Restorable when the disputed lines granted it and then revoked it.
	let mut balance: HashMap<(i32, i32), i32> = HashMap::new();
	let mut line_for: HashMap<(i32, i32), Option<i64>> = HashMap::new();
	for event in &events {
		let key = (event.player_id, event.cosmetic_id);
		match event.kind {
			OwnershipEventKind::Granted => {
				*balance.entry(key).or_default() += 1;
				line_for.insert(key, event.transaction_line_id);
			}
			OwnershipEventKind::Revoked => *balance.entry(key).or_default() -= 1,
		}
	}

	let candidates: Vec<(i32, i32)> = balance
		.into_iter()
		.filter(|(_, count)| *count <= 0)
		.map(|(key, _)| key)
		.collect();

	let held = already_owned(txn, &candidates).await?;
	let restorable: Vec<(i32, i32)> = candidates
		.into_iter()
		.filter(|key| !held.contains(key))
		.collect();
	if restorable.is_empty() {
		return Ok(Grants::new());
	}

	PlayerOwnedCosmetic::insert_many(restorable.iter().map(
		|(player_id, cosmetic_id)| player_owned_cosmetic::ActiveModel {
			player_id: Set(*player_id),
			cosmetic_id: Set(*cosmetic_id),
			acquired_via: Set(TransactionProvider::Paynow),
			transaction_id: Set(Some(transaction.id)),
			transaction_line_id: Set(
				line_for.get(&(*player_id, *cosmetic_id)).copied().flatten(),
			),
			acquired_at: ActiveValue::NotSet,
		},
	))
	.on_conflict(
		OnConflict::columns([
			player_owned_cosmetic::Column::PlayerId,
			player_owned_cosmetic::Column::CosmeticId,
		])
		.do_nothing()
		.to_owned(),
	)
	.exec_without_returning(txn)
	.await?;

	let cosmetics = cosmetics_by_id(
		txn,
		restorable.iter().map(|(_, cosmetic)| *cosmetic).collect(),
	)
	.await?;
	let uuids =
		uuids_by_id(txn, restorable.iter().map(|(player, _)| *player).collect()).await?;

	let mut by_line: HashMap<(i32, Option<i64>), Vec<i32>> = HashMap::new();
	for key @ (player_id, cosmetic_id) in &restorable {
		by_line
			.entry((*player_id, line_for.get(key).copied().flatten()))
			.or_default()
			.push(*cosmetic_id);
	}

	let mut grants = Grants::new();
	for ((player_id, line_id), cosmetic_ids) in &by_line {
		bump_purchase_count(txn, cosmetic_ids, 1).await?;
		crate::database::record_ownership_events(
			txn,
			*player_id,
			cosmetic_ids,
			OwnershipEventKind::Granted,
			TransactionProvider::Paynow,
			Some(transaction.id),
			*line_id,
		)
		.await?;

		let Some(uuid) = uuids.get(player_id) else {
			continue;
		};
		let grant = grants.entry(*uuid).or_default();
		for cosmetic_id in cosmetic_ids {
			if let Some(cosmetic) = cosmetics.get(cosmetic_id) {
				grant.push(cosmetic);
			}
		}
	}

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

	grants.retain(|_, grant| !grant.is_empty());
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

async fn owned_from_line(
	txn: &impl ConnectionTrait,
	line_id: i64,
) -> Result<Vec<i32>, DbErr> {
	Ok(PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::TransactionLineId.eq(line_id))
		.all(txn)
		.await?
		.into_iter()
		.map(|row| row.cosmetic_id)
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

/// Which of these `(player, cosmetic)` pairs the player already holds.
async fn already_owned(
	txn: &impl ConnectionTrait,
	pairs: &[(i32, i32)],
) -> Result<HashSet<(i32, i32)>, DbErr> {
	if pairs.is_empty() {
		return Ok(HashSet::new());
	}

	let players: HashSet<i32> = pairs.iter().map(|(player, _)| *player).collect();
	let cosmetics: HashSet<i32> = pairs.iter().map(|(_, cosmetic)| *cosmetic).collect();
	let wanted: HashSet<(i32, i32)> = pairs.iter().copied().collect();

	Ok(PlayerOwnedCosmetic::find()
		.filter(player_owned_cosmetic::Column::PlayerId.is_in(players))
		.filter(player_owned_cosmetic::Column::CosmeticId.is_in(cosmetics))
		.all(txn)
		.await?
		.into_iter()
		.map(|row| (row.player_id, row.cosmetic_id))
		.filter(|key| wanted.contains(key))
		.collect())
}

async fn cosmetics_by_id(
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

async fn uuids_by_id(
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
