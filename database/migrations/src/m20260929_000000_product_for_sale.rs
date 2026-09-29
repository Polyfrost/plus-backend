use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum ProductSettings {
	Table,
	ForSale,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(ProductSettings::Table)
					.add_column(
						ColumnDef::new(ProductSettings::ForSale)
							.boolean()
							.not_null()
							.default(true),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(ProductSettings::Table)
					.drop_column(ProductSettings::ForSale)
					.to_owned(),
			)
			.await
	}
}
