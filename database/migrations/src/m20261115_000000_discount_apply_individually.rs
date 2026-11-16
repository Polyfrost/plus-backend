use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Discount {
	Table,
	/// Coupons only: off each covered line, or once off the order.
	ApplyIndividually,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(Discount::Table)
					.add_column(
						ColumnDef::new(Discount::ApplyIndividually)
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
					.table(Discount::Table)
					.drop_column(Discount::ApplyIndividually)
					.to_owned(),
			)
			.await
	}
}
