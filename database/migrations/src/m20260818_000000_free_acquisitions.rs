use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum AnalyticsDaily {
	Table,
	CosmeticsAcquiredFree,
}

#[derive(DeriveIden)]
enum AnalyticsCosmeticDaily {
	Table,
	AcquisitionsFree,
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
						ColumnDef::new(AnalyticsDaily::CosmeticsAcquiredFree)
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
						ColumnDef::new(AnalyticsCosmeticDaily::AcquisitionsFree)
							.integer()
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
					.drop_column(AnalyticsCosmeticDaily::AcquisitionsFree)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsDaily::Table)
					.drop_column(AnalyticsDaily::CosmeticsAcquiredFree)
					.to_owned(),
			)
			.await
	}
}
