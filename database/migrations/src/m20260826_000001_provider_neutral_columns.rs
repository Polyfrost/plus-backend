use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	StripeProductId,
	StripePriceId,
	StoreProductId,
}

#[derive(DeriveIden)]
enum Bundles {
	Table,
	StripeProductId,
	StripePriceId,
	StoreProductId,
}

#[derive(DeriveIden)]
enum Transaction {
	Table,
	StripePaymentId,
	ProviderTransactionId,
}

const TRANSACTION_IDX: &str = "transaction_provider_transaction_id_key";
const COSMETIC_IDX: &str = "cosmetic_store_product_id_idx";
const BUNDLES_IDX: &str = "bundles_store_product_id_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// PayNow has no separate price object, so the product/price pair
		// collapses to the single id a checkout line is built from.
		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.rename_column(Cosmetic::StripeProductId, Cosmetic::StoreProductId)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.drop_column(Cosmetic::StripePriceId)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(Bundles::Table)
					.rename_column(Bundles::StripeProductId, Bundles::StoreProductId)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Bundles::Table)
					.drop_column(Bundles::StripePriceId)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.rename_column(
						Transaction::StripePaymentId,
						Transaction::ProviderTransactionId,
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(TRANSACTION_IDX)
					.table(Transaction::Table)
					.col(Transaction::ProviderTransactionId)
					.unique()
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(COSMETIC_IDX)
					.table(Cosmetic::Table)
					.col(Cosmetic::StoreProductId)
					.and_where(Expr::col(Cosmetic::StoreProductId).is_not_null())
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(BUNDLES_IDX)
					.table(Bundles::Table)
					.col(Bundles::StoreProductId)
					.and_where(Expr::col(Bundles::StoreProductId).is_not_null())
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		for (name, table) in [
			(BUNDLES_IDX, Bundles::Table.into_iden()),
			(COSMETIC_IDX, Cosmetic::Table.into_iden()),
			(TRANSACTION_IDX, Transaction::Table.into_iden()),
		] {
			manager
				.drop_index(Index::drop().name(name).table(table).to_owned())
				.await?;
		}

		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.rename_column(
						Transaction::ProviderTransactionId,
						Transaction::StripePaymentId,
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(Bundles::Table)
					.rename_column(Bundles::StoreProductId, Bundles::StripeProductId)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Bundles::Table)
					.add_column(ColumnDef::new(Bundles::StripePriceId).text().null())
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.rename_column(Cosmetic::StoreProductId, Cosmetic::StripeProductId)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Cosmetic::Table)
					.add_column(ColumnDef::new(Cosmetic::StripePriceId).text().null())
					.to_owned(),
			)
			.await
	}
}
