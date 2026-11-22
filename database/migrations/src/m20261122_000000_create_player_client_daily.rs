use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerClientDaily {
	Table,
	PlayerId,
	Day,
	ClientVersion,
	MinecraftVersion,
	Loader,
	Os,
	Country,
}

#[derive(DeriveIden)]
enum PlayerClientInfo {
	Table,
	PlayerId,
	ClientVersion,
	MinecraftVersion,
	Loader,
	Os,
	FirstSeenAt,
}

#[derive(DeriveIden)]
enum PlayerGeo {
	Table,
	PlayerId,
	Country,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PlayerClientDaily::Table)
					.if_not_exists()
					.col(ColumnDef::new(PlayerClientDaily::PlayerId).integer().not_null())
					.col(ColumnDef::new(PlayerClientDaily::Day).date().not_null())
					.col(ColumnDef::new(PlayerClientDaily::ClientVersion).text().null())
					.col(ColumnDef::new(PlayerClientDaily::MinecraftVersion).text().null())
					.col(ColumnDef::new(PlayerClientDaily::Loader).text().null())
					.col(ColumnDef::new(PlayerClientDaily::Os).text().null())
					.col(ColumnDef::new(PlayerClientDaily::Country).char_len(2).null())
					.primary_key(
						Index::create()
							.col(PlayerClientDaily::PlayerId)
							.col(PlayerClientDaily::Day),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerClientDaily::Table, PlayerClientDaily::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		// Only the latest report was kept before, so it stands in for every
		// day since the player was first seen.
		let first_seen_day = Expr::expr(
			Func::cust(Alias::new("timezone"))
				.arg("UTC")
				.arg(Expr::col((PlayerClientInfo::Table, PlayerClientInfo::FirstSeenAt))),
		)
		.cast_as(Alias::new("date"));

		manager
			.exec_stmt(
				Query::insert()
					.into_table(PlayerClientDaily::Table)
					.columns([
						PlayerClientDaily::PlayerId,
						PlayerClientDaily::Day,
						PlayerClientDaily::ClientVersion,
						PlayerClientDaily::MinecraftVersion,
						PlayerClientDaily::Loader,
						PlayerClientDaily::Os,
						PlayerClientDaily::Country,
					])
					.select_from(
						Query::select()
							.column((PlayerClientInfo::Table, PlayerClientInfo::PlayerId))
							.expr(first_seen_day)
							.columns([
								(PlayerClientInfo::Table, PlayerClientInfo::ClientVersion),
								(PlayerClientInfo::Table, PlayerClientInfo::MinecraftVersion),
								(PlayerClientInfo::Table, PlayerClientInfo::Loader),
								(PlayerClientInfo::Table, PlayerClientInfo::Os),
							])
							.column((PlayerGeo::Table, PlayerGeo::Country))
							.from(PlayerClientInfo::Table)
							.left_join(
								PlayerGeo::Table,
								Expr::col((PlayerGeo::Table, PlayerGeo::PlayerId))
									.equals((PlayerClientInfo::Table, PlayerClientInfo::PlayerId)),
							)
							.to_owned(),
					)
					.map_err(|error| DbErr::Migration(error.to_string()))?
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PlayerClientDaily::Table).to_owned())
			.await
	}
}
