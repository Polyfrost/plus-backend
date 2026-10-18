use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
pub struct LinkProvider;

#[derive(DeriveIden, EnumIter)]
pub enum LinkProviderVariants {
	Discord,
}

/// An external account linked to a player, at most one per provider.
#[derive(DeriveIden)]
pub enum LinkedAccount {
	Table,
	UserId,
	Provider,
	ProviderUserId,
	/// The provider-side display name as of linking.
	Username,
	LinkedAt,
}

const PROVIDER_USER_IDX: &str = "linked_account_provider_user_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(LinkProvider)
					.values(LinkProviderVariants::iter())
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(LinkedAccount::Table)
					.if_not_exists()
					.col(ColumnDef::new(LinkedAccount::UserId).integer().not_null())
					.col(
						ColumnDef::new(LinkedAccount::Provider)
							.custom(LinkProvider)
							.not_null(),
					)
					.col(ColumnDef::new(LinkedAccount::ProviderUserId).text().not_null())
					.col(ColumnDef::new(LinkedAccount::Username).text())
					.col(
						ColumnDef::new(LinkedAccount::LinkedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(LinkedAccount::UserId)
							.col(LinkedAccount::Provider),
					)
					.foreign_key(
						ForeignKey::create()
							.from(LinkedAccount::Table, LinkedAccount::UserId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(PROVIDER_USER_IDX)
					.table(LinkedAccount::Table)
					.col(LinkedAccount::Provider)
					.col(LinkedAccount::ProviderUserId)
					.unique()
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(LinkedAccount::Table).to_owned())
			.await?;
		manager
			.drop_type(Type::drop().name(LinkProvider).to_owned())
			.await
	}
}
