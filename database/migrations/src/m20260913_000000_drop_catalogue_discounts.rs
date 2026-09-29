use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	DiscountRate,
}

#[derive(DeriveIden)]
enum Bundles {
	Table,
	DiscountRate,
}

#[derive(DeriveIden)]
enum Transaction {
	Table,
	DiscountRate,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// `base_price` is the list price, and stays one: a rate glued to the
		// row cannot say when a sale runs or what it covers, and folding it in
		// would destroy the number the store needs to strike through. Live
		// discounts move to `sale`; `provision-paynow --sync-prices` puts the
		// list price back on the storefront.
		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.drop_column(Cosmetic::DiscountRate)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Bundles::Table)
					.drop_column(Bundles::DiscountRate)
					.to_owned(),
			)
			.await?;

		// Never written since the move off Stripe; `discount_minor` carries
		// what PayNow actually took off.
		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.drop_column(Transaction::DiscountRate)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// The rates come back empty; they live in `sale` now.
		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.add_column(ColumnDef::new(Cosmetic::DiscountRate).integer().null())
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Bundles::Table)
					.add_column(ColumnDef::new(Bundles::DiscountRate).integer().null())
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.add_column(
						ColumnDef::new(Transaction::DiscountRate).integer().null(),
					)
					.to_owned(),
			)
			.await
	}
}

