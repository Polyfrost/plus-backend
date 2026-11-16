use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AnalyticsCohortRetention {
	Table,
}

/// Cohorts used to count accounts that never logged in. The rollup rebuilds
/// every cohort when it finds this table empty.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.exec_stmt(
				Query::delete()
					.from_table(AnalyticsCohortRetention::Table)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
		Ok(())
	}
}
