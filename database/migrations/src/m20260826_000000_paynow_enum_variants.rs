use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		let db = manager.get_connection();

		// Must be a separate migration from usage of new values (PostgreSQL 55P04).
		for statement in [
			"ALTER TYPE transaction_provider ADD VALUE IF NOT EXISTS 'paynow'",
			"ALTER TYPE transaction_status ADD VALUE IF NOT EXISTS 'chargeback'",
			"ALTER TYPE transaction_status ADD VALUE IF NOT EXISTS 'partially_refunded'",
		] {
			db.execute_unprepared(statement).await?;
		}

		Ok(())
	}

	async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
		Ok(())
	}
}
