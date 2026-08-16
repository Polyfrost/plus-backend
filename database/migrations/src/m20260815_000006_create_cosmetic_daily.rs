use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AnalyticsCosmeticDaily {
	Table,
	Day,
	CosmeticId,
	Views,
	Acquisitions,
	AcquisitionsPaid,
	AcquisitionsGranted,
	ComputedAt,
}

const COSMETIC_DAY_IDX: &str = "analytics_cosmetic_daily_cosmetic_day_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(AnalyticsCosmeticDaily::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(AnalyticsCosmeticDaily::Day)
							.date()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticDaily::CosmeticId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticDaily::Views)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticDaily::Acquisitions)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticDaily::AcquisitionsPaid)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticDaily::AcquisitionsGranted)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticDaily::ComputedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(AnalyticsCosmeticDaily::Day)
							.col(AnalyticsCosmeticDaily::CosmeticId),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(COSMETIC_DAY_IDX)
					.table(AnalyticsCosmeticDaily::Table)
					.col(AnalyticsCosmeticDaily::CosmeticId)
					.col(AnalyticsCosmeticDaily::Day)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(
				Table::drop()
					.table(AnalyticsCosmeticDaily::Table)
					.to_owned(),
			)
			.await
	}
}
