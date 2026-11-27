use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PerfProfile {
	Table,
	Id,
	PlayerId,
	CreatedAt,
	PackHash,
	ClientVersion,
	MinecraftVersion,
	Loader,
	Seconds,
	SizeBytes,
	S3Key,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PerfProfile::Table)
					.if_not_exists()
					.col(ColumnDef::new(PerfProfile::Id).uuid().not_null().primary_key())
					.col(ColumnDef::new(PerfProfile::PlayerId).integer().null())
					.col(
						ColumnDef::new(PerfProfile::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(ColumnDef::new(PerfProfile::PackHash).text().not_null())
					.col(ColumnDef::new(PerfProfile::ClientVersion).text().not_null())
					.col(ColumnDef::new(PerfProfile::MinecraftVersion).text().not_null())
					.col(ColumnDef::new(PerfProfile::Loader).text().not_null())
					.col(ColumnDef::new(PerfProfile::Seconds).integer().not_null())
					.col(ColumnDef::new(PerfProfile::SizeBytes).big_integer().not_null())
					.col(ColumnDef::new(PerfProfile::S3Key).text().not_null())
					.foreign_key(
						ForeignKey::create()
							.from(PerfProfile::Table, PerfProfile::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_perf_profile_player_created_at")
					.table(PerfProfile::Table)
					.col(PerfProfile::PlayerId)
					.col(PerfProfile::CreatedAt)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_perf_profile_created_at")
					.table(PerfProfile::Table)
					.col(PerfProfile::CreatedAt)
					.if_not_exists()
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PerfProfile::Table).to_owned())
			.await
	}
}
