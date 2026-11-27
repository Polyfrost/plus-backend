use sea_orm_migration::{prelude::*, sea_query::Func};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum CosmeticSet {
	Table,
	Id,
	Name,
	Priority,
	EffectAssetId,
	ActiveFrom,
	ActiveUntil,
	CreatedAt,
}

#[derive(DeriveIden)]
enum CosmeticSetRequirement {
	Table,
	Id,
	SetId,
	CosmeticId,
	GroupId,
	VariantKey,
}

#[derive(DeriveIden)]
enum PlayerSetDiscovery {
	Table,
	PlayerId,
	SetId,
	CosmeticIds,
	DiscoveredAt,
}

#[derive(DeriveIden)]
enum Asset {
	Table,
	Id,
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
enum User {
	Table,
	Id,
}

const REQUIREMENT_SET_IDX: &str = "cosmetic_set_requirement_set_id_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(CosmeticSet::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(CosmeticSet::Id)
							.integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(CosmeticSet::Name).text().not_null())
					.col(
						ColumnDef::new(CosmeticSet::Priority)
							.integer()
							.not_null()
							.default(0),
					)
					.col(ColumnDef::new(CosmeticSet::EffectAssetId).integer().null())
					.col(
						ColumnDef::new(CosmeticSet::ActiveFrom)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(CosmeticSet::ActiveUntil)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(CosmeticSet::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.check(
						Expr::col(CosmeticSet::ActiveFrom)
							.is_null()
							.or(Expr::col(CosmeticSet::ActiveUntil).is_null())
							.or(Expr::col(CosmeticSet::ActiveFrom)
								.lt(Expr::col(CosmeticSet::ActiveUntil))),
					)
					.foreign_key(
						ForeignKey::create()
							.from(CosmeticSet::Table, CosmeticSet::EffectAssetId)
							.to(Asset::Table, Asset::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(CosmeticSetRequirement::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(CosmeticSetRequirement::Id)
							.integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(CosmeticSetRequirement::SetId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(CosmeticSetRequirement::CosmeticId)
							.integer()
							.null(),
					)
					.col(
						ColumnDef::new(CosmeticSetRequirement::GroupId)
							.integer()
							.null(),
					)
					.col(
						ColumnDef::new(CosmeticSetRequirement::VariantKey)
							.text()
							.null(),
					)
					.check(
						Expr::expr(Func::cust(Alias::new("num_nonnulls")).args([
							SimpleExpr::from(Expr::col(
								CosmeticSetRequirement::CosmeticId,
							)),
							Expr::col(CosmeticSetRequirement::GroupId).into(),
						]))
						.eq(1),
					)
					.check(
						Expr::col(CosmeticSetRequirement::VariantKey)
							.is_null()
							.or(Expr::col(CosmeticSetRequirement::GroupId).is_not_null()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								CosmeticSetRequirement::Table,
								CosmeticSetRequirement::SetId,
							)
							.to(CosmeticSet::Table, CosmeticSet::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								CosmeticSetRequirement::Table,
								CosmeticSetRequirement::CosmeticId,
							)
							.to(Cosmetic::Table, Cosmetic::Id),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								CosmeticSetRequirement::Table,
								CosmeticSetRequirement::GroupId,
							)
							.to(CosmeticGroup::Table, CosmeticGroup::Id),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(REQUIREMENT_SET_IDX)
					.table(CosmeticSetRequirement::Table)
					.col(CosmeticSetRequirement::SetId)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PlayerSetDiscovery::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerSetDiscovery::PlayerId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(PlayerSetDiscovery::SetId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(PlayerSetDiscovery::CosmeticIds)
							.array(ColumnType::Integer)
							.not_null(),
					)
					.col(
						ColumnDef::new(PlayerSetDiscovery::DiscoveredAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(PlayerSetDiscovery::PlayerId)
							.col(PlayerSetDiscovery::SetId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerSetDiscovery::Table, PlayerSetDiscovery::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerSetDiscovery::Table, PlayerSetDiscovery::SetId)
							.to(CosmeticSet::Table, CosmeticSet::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PlayerSetDiscovery::Table).to_owned())
			.await?;
		manager
			.drop_table(
				Table::drop()
					.table(CosmeticSetRequirement::Table)
					.to_owned(),
			)
			.await?;
		manager
			.drop_table(Table::drop().table(CosmeticSet::Table).to_owned())
			.await
	}
}
