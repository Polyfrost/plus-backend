use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerGeo {
	Table,
	PlayerId,
	Country,
	FirstSeenAt,
	LastSeenAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PlayerGeo::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerGeo::PlayerId)
							.integer()
							.not_null()
							.primary_key(),
					)
					.col(ColumnDef::new(PlayerGeo::Country).char_len(2).not_null())
					.col(
						ColumnDef::new(PlayerGeo::FirstSeenAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PlayerGeo::LastSeenAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerGeo::Table, PlayerGeo::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PlayerGeo::Table).to_owned())
			.await
	}
}
