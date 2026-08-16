use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AnalyticsCohortRetention {
	Table,
	CohortDay,
	DayOffset,
	CohortSize,
	Retained,
	ComputedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(AnalyticsCohortRetention::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(AnalyticsCohortRetention::CohortDay)
							.date()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCohortRetention::DayOffset)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCohortRetention::CohortSize)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCohortRetention::Retained)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCohortRetention::ComputedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(AnalyticsCohortRetention::CohortDay)
							.col(AnalyticsCohortRetention::DayOffset),
					)
					.to_owned(),
			)
			.await?;

		// "D7 over time" reads one offset across many cohorts, which the
		// primary key orders the wrong way for.
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name("analytics_cohort_retention_offset_day_idx")
					.table(AnalyticsCohortRetention::Table)
					.col(AnalyticsCohortRetention::DayOffset)
					.col(AnalyticsCohortRetention::CohortDay)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(
				Table::drop()
					.table(AnalyticsCohortRetention::Table)
					.to_owned(),
			)
			.await
	}
}
