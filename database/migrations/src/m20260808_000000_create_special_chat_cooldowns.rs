use sea_orm_migration::prelude::*;

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum SpecialChatCooldowns {
	Table,
	SenderId,
	TargetId,
	LastSentAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(SpecialChatCooldowns::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(SpecialChatCooldowns::SenderId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(SpecialChatCooldowns::TargetId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(SpecialChatCooldowns::LastSentAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.primary_key(
						Index::create()
							.col(SpecialChatCooldowns::SenderId)
							.col(SpecialChatCooldowns::TargetId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(SpecialChatCooldowns::Table, SpecialChatCooldowns::SenderId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(SpecialChatCooldowns::Table, SpecialChatCooldowns::TargetId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(SpecialChatCooldowns::Table).to_owned())
			.await
	}
}
