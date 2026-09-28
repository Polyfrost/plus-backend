use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerMod {
	Table,
	PlayerId,
	ModId,
	Version,
	ReportedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PlayerMod::Table)
					.if_not_exists()
					.col(ColumnDef::new(PlayerMod::PlayerId).integer().not_null())
					.col(ColumnDef::new(PlayerMod::ModId).text().not_null())
					.col(ColumnDef::new(PlayerMod::Version).text().not_null())
					.col(
						ColumnDef::new(PlayerMod::ReportedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(PlayerMod::PlayerId)
							.col(PlayerMod::ModId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerMod::Table, PlayerMod::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_player_mod_reported_at")
					.table(PlayerMod::Table)
					.col(PlayerMod::ReportedAt)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PlayerMod::Table).to_owned())
			.await
	}
}
