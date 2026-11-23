use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum DailyLogin {
	Table,
	PlayerId,
	Day,
}

#[derive(DeriveIden)]
enum AnalyticsDaily {
	Table,
	Logins,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(DailyLogin::Table)
					.if_not_exists()
					.col(ColumnDef::new(DailyLogin::PlayerId).integer().not_null())
					.col(ColumnDef::new(DailyLogin::Day).date().not_null())
					.primary_key(
						Index::create().col(DailyLogin::PlayerId).col(DailyLogin::Day),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DailyLogin::Table, DailyLogin::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_daily_login_day")
					.table(DailyLogin::Table)
					.col(DailyLogin::Day)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsDaily::Table)
					.add_column_if_not_exists(
						ColumnDef::new(AnalyticsDaily::Logins)
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
					.drop_column(AnalyticsDaily::Logins)
					.to_owned(),
			)
			.await?;

		manager
			.drop_table(Table::drop().table(DailyLogin::Table).to_owned())
			.await
	}
}
