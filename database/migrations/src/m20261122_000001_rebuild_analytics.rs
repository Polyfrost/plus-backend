use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum AnalyticsJobState {
	Table,
	JobName,
}

#[derive(DeriveIden)]
enum AnalyticsCohortRetention {
	Table,
}

const ANALYTICS_DAILY_JOB: &str = "analytics_daily";

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Cohorts moved from signup day to first-login day, so the old cells would
/// linger under days that no longer have a cohort. Dropping the watermark
/// makes the rollup recompute every day under the new definitions.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.exec_stmt(
				Query::delete()
					.from_table(AnalyticsCohortRetention::Table)
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

	async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
		Ok(())
	}
}
