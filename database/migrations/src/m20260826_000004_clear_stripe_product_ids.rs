use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	StoreProductId,
}

#[derive(DeriveIden)]
enum Bundles {
	Table,
	StoreProductId,
}

/// Stripe product ids all carry this prefix; a PayNow flake id is only digits,
/// so this cannot match one that has already been provisioned.
const STRIPE_PREFIX: &str = "prod%";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// Renaming the column carried the old ids across, and provisioning
		// reads a non-null id as "already on the storefront" and skips the row.
		manager
			.exec_stmt(
				Query::update()
					.table(Cosmetic::Table)
					.value(Cosmetic::StoreProductId, Option::<String>::None)
					.and_where(Expr::col(Cosmetic::StoreProductId).like(STRIPE_PREFIX))
					.to_owned(),
			)
			.await?;

		manager
			.exec_stmt(
				Query::update()
					.table(Bundles::Table)
					.value(Bundles::StoreProductId, Option::<String>::None)
					.and_where(Expr::col(Bundles::StoreProductId).like(STRIPE_PREFIX))
					.to_owned(),
			)
			.await
	}

	async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
		// The ids belonged to a store that is no longer in use.
		Ok(())
	}
}
