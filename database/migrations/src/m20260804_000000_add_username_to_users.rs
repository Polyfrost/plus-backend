use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveIden)]
enum UserExtra {
	Username,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(User::Table)
					.add_column(ColumnDef::new(UserExtra::Username).string().null())
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(User::Table)
					.drop_column(UserExtra::Username)
					.to_owned(),
			)
			.await
	}
}
