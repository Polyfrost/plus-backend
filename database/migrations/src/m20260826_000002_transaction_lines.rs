use sea_orm_migration::prelude::*;

use crate::{
	m20250917_163702_create_users_table::User,
	m20260815_000007_create_ownership_events::CosmeticOwnershipEvent,
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum TransactionLine {
	Table,
	Id,
	TransactionId,
	ProviderLineId,
	ProductId,
	BundleId,
	CosmeticGroupId,
	CosmeticId,
	RecipientId,
	Quantity,
	PriceMinor,
	DiscountMinor,
	SubtotalMinor,
	TaxMinor,
	TotalMinor,
	Currency,
	Status,
	ReturnedMinor,
	ReturnedAt,
	CreatedAt,
}

#[derive(DeriveIden)]
enum Transaction {
	Table,
	Id,
	RefundedMinor,
	ChargedBackAt,
}

#[derive(DeriveIden)]
enum PlayerOwnedCosmetic {
	Table,
	TransactionLineId,
}

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum CosmeticGroup {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Bundles {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum UserExt {
	#[sea_orm(iden = "user")]
	Table,
	ChargebackCount,
	PaynowCustomerId,
}

#[derive(DeriveIden)]
enum PaynowWebhookEvent {
	Table,
	EventId,
	EventType,
	ReceivedAt,
}

#[derive(DeriveIden)]
enum TransactionStatus {
	#[sea_orm(iden = "transaction_status")]
	Enum,
}

const LINE_TRANSACTION_IDX: &str = "transaction_line_transaction_id_idx";
const LINE_PROVIDER_IDX: &str = "transaction_line_provider_line_id_key";
const LINE_RETURN_IDX: &str = "transaction_line_status_returned_at_idx";
const OWNED_LINE_IDX: &str = "player_owned_cosmetic_transaction_line_id_idx";
const EVENT_LINE_IDX: &str = "cosmetic_ownership_event_transaction_line_id_idx";
const CUSTOMER_IDX: &str = "user_paynow_customer_id_key";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(TransactionLine::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(TransactionLine::Id)
							.big_integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(TransactionLine::TransactionId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(TransactionLine::ProviderLineId)
							.text()
							.not_null(),
					)
					.col(ColumnDef::new(TransactionLine::ProductId).text().not_null())
					.col(ColumnDef::new(TransactionLine::BundleId).integer().null())
					.col(
						ColumnDef::new(TransactionLine::CosmeticGroupId)
							.integer()
							.null(),
					)
					.col(ColumnDef::new(TransactionLine::CosmeticId).integer().null())
					.col(
						ColumnDef::new(TransactionLine::RecipientId)
							.integer()
							.null(),
					)
					.col(
						ColumnDef::new(TransactionLine::Quantity)
							.integer()
							.not_null()
							.default(1),
					)
					.col(
						ColumnDef::new(TransactionLine::PriceMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(TransactionLine::DiscountMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(TransactionLine::SubtotalMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(TransactionLine::TaxMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(TransactionLine::TotalMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(ColumnDef::new(TransactionLine::Currency).text().not_null())
					.col(
						ColumnDef::new(TransactionLine::Status)
							.custom(TransactionStatus::Enum)
							.not_null()
							.default("completed"),
					)
					.col(
						ColumnDef::new(TransactionLine::ReturnedMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(TransactionLine::ReturnedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(TransactionLine::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(TransactionLine::Table, TransactionLine::TransactionId)
							.to(Transaction::Table, Transaction::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(TransactionLine::Table, TransactionLine::BundleId)
							.to(Bundles::Table, Bundles::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								TransactionLine::Table,
								TransactionLine::CosmeticGroupId,
							)
							.to(CosmeticGroup::Table, CosmeticGroup::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.foreign_key(
						ForeignKey::create()
							.from(TransactionLine::Table, TransactionLine::CosmeticId)
							.to(Cosmetic::Table, Cosmetic::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.foreign_key(
						ForeignKey::create()
							.from(TransactionLine::Table, TransactionLine::RecipientId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(LINE_PROVIDER_IDX)
					.table(TransactionLine::Table)
					.col(TransactionLine::ProviderLineId)
					.unique()
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(LINE_TRANSACTION_IDX)
					.table(TransactionLine::Table)
					.col(TransactionLine::TransactionId)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(LINE_RETURN_IDX)
					.table(TransactionLine::Table)
					.col(TransactionLine::Status)
					.col(TransactionLine::ReturnedAt)
					.to_owned(),
			)
			.await?;

		// A partial refund revokes exactly one line's cosmetics.
		manager
			.alter_table(
				Table::alter()
					.table(PlayerOwnedCosmetic::Table)
					.add_column(
						ColumnDef::new(PlayerOwnedCosmetic::TransactionLineId)
							.big_integer()
							.null(),
					)
					.add_foreign_key(
						TableForeignKey::new()
							.from_tbl(PlayerOwnedCosmetic::Table)
							.from_col(PlayerOwnedCosmetic::TransactionLineId)
							.to_tbl(TransactionLine::Table)
							.to_col(TransactionLine::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(OWNED_LINE_IDX)
					.table(PlayerOwnedCosmetic::Table)
					.col(PlayerOwnedCosmetic::TransactionLineId)
					.and_where(
						Expr::col(PlayerOwnedCosmetic::TransactionLineId).is_not_null(),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(CosmeticOwnershipEvent::Table)
					.add_column(
						ColumnDef::new(PlayerOwnedCosmetic::TransactionLineId)
							.big_integer()
							.null(),
					)
					.add_foreign_key(
						TableForeignKey::new()
							.from_tbl(CosmeticOwnershipEvent::Table)
							.from_col(PlayerOwnedCosmetic::TransactionLineId)
							.to_tbl(TransactionLine::Table)
							.to_col(TransactionLine::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(EVENT_LINE_IDX)
					.table(CosmeticOwnershipEvent::Table)
					.col(PlayerOwnedCosmetic::TransactionLineId)
					.and_where(
						Expr::col(PlayerOwnedCosmetic::TransactionLineId).is_not_null(),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.add_column(
						ColumnDef::new(Transaction::RefundedMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(Transaction::ChargedBackAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(UserExt::Table)
					.add_column(
						ColumnDef::new(UserExt::ChargebackCount)
							.integer()
							.not_null()
							.default(0),
					)
					.add_column(ColumnDef::new(UserExt::PaynowCustomerId).text().null())
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(CUSTOMER_IDX)
					.table(UserExt::Table)
					.col(UserExt::PaynowCustomerId)
					.unique()
					.and_where(Expr::col(UserExt::PaynowCustomerId).is_not_null())
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PaynowWebhookEvent::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PaynowWebhookEvent::EventId)
							.text()
							.not_null()
							.primary_key(),
					)
					.col(
						ColumnDef::new(PaynowWebhookEvent::EventType)
							.text()
							.not_null(),
					)
					.col(
						ColumnDef::new(PaynowWebhookEvent::ReceivedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PaynowWebhookEvent::Table).to_owned())
			.await?;

		manager
			.drop_index(
				Index::drop()
					.name(CUSTOMER_IDX)
					.table(UserExt::Table)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(UserExt::Table)
					.drop_column(UserExt::PaynowCustomerId)
					.drop_column(UserExt::ChargebackCount)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.drop_column(Transaction::ChargedBackAt)
					.drop_column(Transaction::RefundedMinor)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(CosmeticOwnershipEvent::Table)
					.drop_column(PlayerOwnedCosmetic::TransactionLineId)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(PlayerOwnedCosmetic::Table)
					.drop_column(PlayerOwnedCosmetic::TransactionLineId)
					.to_owned(),
			)
			.await?;

		manager
			.drop_table(Table::drop().table(TransactionLine::Table).to_owned())
			.await
	}
}
