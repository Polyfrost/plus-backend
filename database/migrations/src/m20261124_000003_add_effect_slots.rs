use sea_orm_migration::prelude::*;

use crate::enum_guard::refuse_if_used;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		let db = manager.get_connection();

		for statement in [
			"ALTER TYPE cosmetic_type ADD VALUE IF NOT EXISTS 'effect'",
			"ALTER TYPE body_slot ADD VALUE IF NOT EXISTS 'effect_a'",
			"ALTER TYPE body_slot ADD VALUE IF NOT EXISTS 'effect_b'",
		] {
			db.execute_unprepared(statement).await?;
		}

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		refuse_if_used(manager, "cosmetic_type", &["effect"]).await?;
		refuse_if_used(manager, "body_slot", &["effect_a", "effect_b"]).await
	}
}
