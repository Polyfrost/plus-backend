use sea_orm_migration::prelude::*;

use crate::m20260731_000000_create_social_tables::{GroupMessages, SessionInvites};

#[derive(DeriveIden)]
enum GroupMessagesExtra {
	SessionInviteId,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				TableAlterStatement::new()
					.table(GroupMessages::Table)
					.add_column_if_not_exists(
						ColumnDef::new(GroupMessagesExtra::SessionInviteId)
							.integer()
							.null()
							.to_owned(),
					)
					.add_foreign_key(
						TableForeignKey::new()
							.name("fk_group_messages_session_invite_id")
							.from_tbl(GroupMessages::Table)
							.from_col(GroupMessagesExtra::SessionInviteId)
							.to_tbl(SessionInvites::Table)
							.to_col(SessionInvites::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				TableAlterStatement::new()
					.table(GroupMessages::Table)
					.drop_foreign_key("fk_group_messages_session_invite_id")
					.drop_column(GroupMessagesExtra::SessionInviteId)
					.to_owned(),
			)
			.await
	}
}
