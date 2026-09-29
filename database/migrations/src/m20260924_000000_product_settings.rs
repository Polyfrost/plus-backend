use sea_orm_migration::{prelude::*, sea_query::Func};

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Keyed by whichever row is sold as the product: a lone cosmetic, a group,
/// or a bundle.
#[derive(DeriveIden)]
enum ProductSettings {
	Table,
	Id,
	CosmeticId,
	CosmeticGroupId,
	BundleId,
	AvailableFrom,
	AvailableUntil,
	StockLimit,
	CustomerLimit,
	CustomerLimitDays,
	GiftingDisabled,
	CouponsDisabled,
	RequiresAll,
	ExpiresAfterDays,
}

#[derive(DeriveIden)]
enum ProductRequirement {
	Table,
	SettingsId,
	CosmeticId,
}

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum CosmeticGroup {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Bundles {
	Table,
	Id,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(ProductSettings::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(ProductSettings::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(ProductSettings::CosmeticId)
							.integer()
							.null()
							.unique_key(),
					)
					.col(
						ColumnDef::new(ProductSettings::CosmeticGroupId)
							.integer()
							.null()
							.unique_key(),
					)
					.col(
						ColumnDef::new(ProductSettings::BundleId)
							.integer()
							.null()
							.unique_key(),
					)
					.col(
						ColumnDef::new(ProductSettings::AvailableFrom)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(ProductSettings::AvailableUntil)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(ColumnDef::new(ProductSettings::StockLimit).integer().null())
					.col(ColumnDef::new(ProductSettings::CustomerLimit).integer().null())
					.col(
						ColumnDef::new(ProductSettings::CustomerLimitDays)
							.integer()
							.null(),
					)
					.col(
						ColumnDef::new(ProductSettings::GiftingDisabled)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(
						ColumnDef::new(ProductSettings::CouponsDisabled)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(
						ColumnDef::new(ProductSettings::RequiresAll)
							.boolean()
							.not_null()
							.default(true),
					)
					.col(
						ColumnDef::new(ProductSettings::ExpiresAfterDays)
							.integer()
							.null(),
					)
					.check(
						Expr::expr(Func::cust(Alias::new("num_nonnulls")).args([
							SimpleExpr::from(Expr::col(ProductSettings::CosmeticId)),
							Expr::col(ProductSettings::CosmeticGroupId).into(),
							Expr::col(ProductSettings::BundleId).into(),
						]))
						.eq(1),
					)
					.foreign_key(
						ForeignKey::create()
							.from(ProductSettings::Table, ProductSettings::CosmeticId)
							.to(Cosmetic::Table, Cosmetic::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(ProductSettings::Table, ProductSettings::CosmeticGroupId)
							.to(CosmeticGroup::Table, CosmeticGroup::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(ProductSettings::Table, ProductSettings::BundleId)
							.to(Bundles::Table, Bundles::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(ProductRequirement::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(ProductRequirement::SettingsId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(ProductRequirement::CosmeticId)
							.integer()
							.not_null(),
					)
					.primary_key(
						Index::create()
							.col(ProductRequirement::SettingsId)
							.col(ProductRequirement::CosmeticId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(ProductRequirement::Table, ProductRequirement::SettingsId)
							.to(ProductSettings::Table, ProductSettings::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(ProductRequirement::Table, ProductRequirement::CosmeticId)
							.to(Cosmetic::Table, Cosmetic::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(ProductRequirement::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(ProductSettings::Table).to_owned())
			.await
	}
}
