use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
pub enum Cosmetic {
	Table,
	PreviewAssetId,
}

#[derive(DeriveIden)]
pub enum Asset {
	Table,
	Id,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				TableAlterStatement::new()
					.table(Cosmetic::Table)
					.add_column_if_not_exists(
						ColumnDef::new(Cosmetic::PreviewAssetId).null().integer(),
					)
					.add_foreign_key(
						TableForeignKey::new()
							.name("fk_cosmetic_preview_asset")
							.from_tbl(Cosmetic::Table)
							.from_col(Cosmetic::PreviewAssetId)
							.to_tbl(Asset::Table)
							.to_col(Asset::Id)
							.on_delete(ForeignKeyAction::SetNull)
							.on_update(ForeignKeyAction::NoAction),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				TableAlterStatement::new()
					.table(Cosmetic::Table)
					.drop_foreign_key("fk_cosmetic_preview_asset")
					.drop_column(Cosmetic::PreviewAssetId)
					.to_owned(),
			)
			.await
	}
}
