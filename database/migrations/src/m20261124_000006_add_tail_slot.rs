use sea_orm_migration::prelude::*;

use crate::enum_guard::refuse_if_used;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared("ALTER TYPE body_slot ADD VALUE IF NOT EXISTS 'tail'")
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		refuse_if_used(manager, "body_slot", &["tail"]).await
	}
}
