use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum DmDelegates {
	Table,
	OwnerId,
	DelegateId,
}

#[derive(DeriveIden)]
enum GroupMessages {
	Table,
	SentById,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(DmDelegates::Table)
					.if_not_exists()
					.col(ColumnDef::new(DmDelegates::OwnerId).integer().not_null())
					.col(ColumnDef::new(DmDelegates::DelegateId).integer().not_null())
					.primary_key(
						Index::create()
							.col(DmDelegates::OwnerId)
							.col(DmDelegates::DelegateId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DmDelegates::Table, DmDelegates::OwnerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DmDelegates::Table, DmDelegates::DelegateId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(GroupMessages::Table)
					.add_column(ColumnDef::new(GroupMessages::SentById).integer().null())
					.add_foreign_key(
						TableForeignKey::new()
							.from_tbl(GroupMessages::Table)
							.from_col(GroupMessages::SentById)
							.to_tbl(User::Table)
							.to_col(User::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(GroupMessages::Table)
					.drop_column(GroupMessages::SentById)
					.to_owned(),
			)
			.await?;
		manager
			.drop_table(Table::drop().table(DmDelegates::Table).to_owned())
			.await
	}
}
