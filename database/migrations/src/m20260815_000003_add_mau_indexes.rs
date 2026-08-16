use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum MonthlyActiveLogin {
	Table,
	Month,
	LastLoginAt,
}

const MONTH_IDX: &str = "monthly_active_login_month_idx";
const LAST_LOGIN_IDX: &str = "monthly_active_login_last_login_at_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(MONTH_IDX)
					.table(MonthlyActiveLogin::Table)
					.col(MonthlyActiveLogin::Month)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(LAST_LOGIN_IDX)
					.table(MonthlyActiveLogin::Table)
					.col(MonthlyActiveLogin::LastLoginAt)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_index(Index::drop().if_exists().name(LAST_LOGIN_IDX).to_owned())
			.await?;

		manager
			.drop_index(Index::drop().if_exists().name(MONTH_IDX).to_owned())
			.await
	}
}
