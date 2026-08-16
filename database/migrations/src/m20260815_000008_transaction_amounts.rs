use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Transaction {
	Table,
	Amount,
	AmountMinor,
	Currency,
	DiscountMinor,
	RefundedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// The old `amount` was a float and was never written. Money moves to
		// integer minor units alongside the currency it was charged in.
		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.add_column(
						ColumnDef::new(Transaction::AmountMinor)
							.big_integer()
							.null(),
					)
					.add_column(ColumnDef::new(Transaction::Currency).text().null())
					.add_column(
						ColumnDef::new(Transaction::DiscountMinor)
							.big_integer()
							.null(),
					)
					.add_column(
						ColumnDef::new(Transaction::RefundedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.drop_column(Transaction::Amount)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.add_column(ColumnDef::new(Transaction::Amount).float().null())
					.drop_column(Transaction::RefundedAt)
					.drop_column(Transaction::DiscountMinor)
					.drop_column(Transaction::Currency)
					.drop_column(Transaction::AmountMinor)
					.to_owned(),
			)
			.await
	}
}
