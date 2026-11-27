use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Asset {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum PlayerCosmeticColors {
	Table,
	PlayerId,
	CosmeticId,
	Slot,
	Color,
	UpdatedAt,
}

#[derive(DeriveIden)]
enum AssetColorSlot {
	Table,
	AssetId,
	Slot,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PlayerCosmeticColors::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerCosmeticColors::PlayerId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(PlayerCosmeticColors::CosmeticId)
							.integer()
							.not_null(),
					)
					.col(ColumnDef::new(PlayerCosmeticColors::Slot).text().not_null())
					.col(
						ColumnDef::new(PlayerCosmeticColors::Color)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(PlayerCosmeticColors::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(PlayerCosmeticColors::PlayerId)
							.col(PlayerCosmeticColors::CosmeticId)
							.col(PlayerCosmeticColors::Slot),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								PlayerCosmeticColors::Table,
								PlayerCosmeticColors::PlayerId,
							)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								PlayerCosmeticColors::Table,
								PlayerCosmeticColors::CosmeticId,
							)
							.to(Cosmetic::Table, Cosmetic::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_player_cosmetic_colors_cosmetic")
					.table(PlayerCosmeticColors::Table)
					.col(PlayerCosmeticColors::CosmeticId)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(AssetColorSlot::Table)
					.if_not_exists()
					.col(ColumnDef::new(AssetColorSlot::AssetId).integer().not_null())
					.col(ColumnDef::new(AssetColorSlot::Slot).text().not_null())
					.primary_key(
						Index::create()
							.col(AssetColorSlot::AssetId)
							.col(AssetColorSlot::Slot),
					)
					.foreign_key(
						ForeignKey::create()
							.from(AssetColorSlot::Table, AssetColorSlot::AssetId)
							.to(Asset::Table, Asset::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(AssetColorSlot::Table).to_owned())
			.await?;

		manager
			.drop_table(Table::drop().table(PlayerCosmeticColors::Table).to_owned())
			.await
	}
}
