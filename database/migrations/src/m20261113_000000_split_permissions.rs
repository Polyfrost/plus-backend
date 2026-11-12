use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// VIEW_REVENUE (1 << 10) splits off VIEW_ANALYTICS (1 << 2), and VIEW_ORDERS,
/// MANAGE_DISCOUNTS and MANAGE_REFERRALS (1 << 11..=13) split off MANAGE_STORE
/// (1 << 4). Whoever held the broad flag keeps everything it used to cover.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(
				"UPDATE role SET permissions = permissions | 1024 WHERE permissions & 4 <> 0;
				 UPDATE role SET permissions = permissions | 14336 WHERE permissions & 16 <> 0;
				 UPDATE api_token SET permissions = permissions | 1024 WHERE permissions & 4 <> 0;
				 UPDATE api_token SET permissions = permissions | 14336 WHERE permissions & 16 <> 0;",
			)
			.await?;
		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(
				"UPDATE role SET permissions = permissions & ~15360;
				 UPDATE api_token SET permissions = permissions & ~15360;",
			)
			.await?;
		Ok(())
	}
}
