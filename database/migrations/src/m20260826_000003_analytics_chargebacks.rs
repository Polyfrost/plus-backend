use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum AnalyticsDaily {
	Table,
	ChargebackAmountMinor,
	TransactionsChargedBack,
	TransactionsPartiallyRefunded,
}

#[derive(DeriveIden)]
enum AnalyticsCosmeticDaily {
	Table,
	RevenueMinor,
	Refunded,
	ChargedBack,
	RefundedMinor,
	ChargedBackMinor,
}

#[derive(DeriveIden)]
enum AnalyticsJobState {
	Table,
	JobName,
}

const ANALYTICS_DAILY_JOB: &str = "analytics_daily";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsDaily::Table)
					.add_column(
						ColumnDef::new(AnalyticsDaily::ChargebackAmountMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsDaily::TransactionsChargedBack)
							.integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsDaily::TransactionsPartiallyRefunded)
							.integer()
							.not_null()
							.default(0),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsCosmeticDaily::Table)
					.add_column(
						ColumnDef::new(AnalyticsCosmeticDaily::RevenueMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsCosmeticDaily::Refunded)
							.integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsCosmeticDaily::ChargedBack)
							.integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsCosmeticDaily::RefundedMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.add_column(
						ColumnDef::new(AnalyticsCosmeticDaily::ChargedBackMinor)
							.big_integer()
							.not_null()
							.default(0),
					)
					.to_owned(),
			)
			.await?;

		manager
			.exec_stmt(
				Query::delete()
					.from_table(AnalyticsJobState::Table)
					.and_where(
						Expr::col(AnalyticsJobState::JobName).eq(ANALYTICS_DAILY_JOB),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsCosmeticDaily::Table)
					.drop_column(AnalyticsCosmeticDaily::ChargedBackMinor)
					.drop_column(AnalyticsCosmeticDaily::RefundedMinor)
					.drop_column(AnalyticsCosmeticDaily::ChargedBack)
					.drop_column(AnalyticsCosmeticDaily::Refunded)
					.drop_column(AnalyticsCosmeticDaily::RevenueMinor)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsDaily::Table)
					.drop_column(AnalyticsDaily::TransactionsPartiallyRefunded)
					.drop_column(AnalyticsDaily::TransactionsChargedBack)
					.drop_column(AnalyticsDaily::ChargebackAmountMinor)
					.to_owned(),
			)
			.await
	}
}
