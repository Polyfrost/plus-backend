use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	VariantKey,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		let db = manager.get_connection();

		for statement in [
			"ALTER TYPE cosmetic_type ADD VALUE IF NOT EXISTS 'nameplate'",
			"ALTER TYPE body_slot ADD VALUE IF NOT EXISTS 'nameplate'",
		] {
			db.execute_unprepared(statement).await?;
		}

		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.add_column_if_not_exists(
						ColumnDef::new(Cosmetic::VariantKey).text().null(),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.drop_column(Cosmetic::VariantKey)
					.to_owned(),
			)
			.await
	}
}
