use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AnalyticsDaily {
	Table,
	GrossRevenueMinor,
	RefundAmountMinor,
	DiscountAmountMinor,
	PayingUsers,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsDaily::Table)
					.add_column(
						ColumnDef::new(AnalyticsDaily::GrossRevenueMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					// Booked on the refund date, not the original sale date: a
					// refund can land long outside any recompute window.
					.add_column(
						ColumnDef::new(AnalyticsDaily::RefundAmountMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsDaily::DiscountAmountMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsDaily::PayingUsers)
							.integer()
							.not_null()
							.default(0),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsDaily::Table)
					.drop_column(AnalyticsDaily::PayingUsers)
					.drop_column(AnalyticsDaily::DiscountAmountMinor)
					.drop_column(AnalyticsDaily::RefundAmountMinor)
					.drop_column(AnalyticsDaily::GrossRevenueMinor)
					.to_owned(),
			)
			.await
	}
}
