use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerClientInfo {
	Table,
	PlayerId,
	ClientVersion,
	MinecraftVersion,
	Loader,
	Os,
	OsVersion,
	JavaVersion,
	FirstSeenAt,
	LastSeenAt,
}

#[derive(DeriveIden)]
enum AnalyticsClientDaily {
	Table,
	Day,
	ClientVersion,
	MinecraftVersion,
	Loader,
	Os,
	ActivePlayers,
	ComputedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PlayerClientInfo::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerClientInfo::PlayerId)
							.integer()
							.not_null()
							.primary_key(),
					)
					.col(
						ColumnDef::new(PlayerClientInfo::ClientVersion)
							.text()
							.null(),
					)
					.col(
						ColumnDef::new(PlayerClientInfo::MinecraftVersion)
							.text()
							.null(),
					)
					.col(ColumnDef::new(PlayerClientInfo::Loader).text().null())
					.col(ColumnDef::new(PlayerClientInfo::Os).text().null())
					.col(ColumnDef::new(PlayerClientInfo::OsVersion).text().null())
					.col(ColumnDef::new(PlayerClientInfo::JavaVersion).text().null())
					.col(
						ColumnDef::new(PlayerClientInfo::FirstSeenAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PlayerClientInfo::LastSeenAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerClientInfo::Table, PlayerClientInfo::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(AnalyticsClientDaily::Table)
					.if_not_exists()
					.col(ColumnDef::new(AnalyticsClientDaily::Day).date().not_null())
					.col(
						ColumnDef::new(AnalyticsClientDaily::ClientVersion)
							.text()
							.not_null()
							.default(""),
					)
					.col(
						ColumnDef::new(AnalyticsClientDaily::MinecraftVersion)
							.text()
							.not_null()
							.default(""),
					)
					.col(
						ColumnDef::new(AnalyticsClientDaily::Loader)
							.text()
							.not_null()
							.default(""),
					)
					.col(
						ColumnDef::new(AnalyticsClientDaily::Os)
							.text()
							.not_null()
							.default(""),
					)
					.col(
						ColumnDef::new(AnalyticsClientDaily::ActivePlayers)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsClientDaily::ComputedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(AnalyticsClientDaily::Day)
							.col(AnalyticsClientDaily::ClientVersion)
							.col(AnalyticsClientDaily::MinecraftVersion)
							.col(AnalyticsClientDaily::Loader)
							.col(AnalyticsClientDaily::Os),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(AnalyticsClientDaily::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(PlayerClientInfo::Table).to_owned())
			.await
	}
}
