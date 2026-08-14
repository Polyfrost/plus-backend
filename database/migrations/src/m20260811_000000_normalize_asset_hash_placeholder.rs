use sea_orm_migration::prelude::*;

// Replaces a legacy hash introduced in an earlier commit from md5 to sha256
// to be consistent with the rest of the logic

const LEGACY_PLACEHOLDER: &str = "37a6259cc0c1dae299a7866489dff0bd";
const PLACEHOLDER: &str = "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(&format!(
				"UPDATE asset
				 SET hash = '{PLACEHOLDER}'
				 WHERE hash IS NULL OR hash = '{LEGACY_PLACEHOLDER}';"
			))
			.await
			.map(|_| ())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(&format!(
				"UPDATE asset
				 SET hash = '{LEGACY_PLACEHOLDER}'
				 WHERE hash = '{PLACEHOLDER}';"
			))
			.await
			.map(|_| ())
	}
}
