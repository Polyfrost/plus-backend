use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerSetDiscovery {
	Table,
	DiscoveredAt,
	DeliveredAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(PlayerSetDiscovery::Table)
					.add_column(
						ColumnDef::new(PlayerSetDiscovery::DeliveredAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.to_owned(),
			)
			.await?;

		let backfill = Query::update()
			.table(PlayerSetDiscovery::Table)
			.value(
				PlayerSetDiscovery::DeliveredAt,
				Expr::col(PlayerSetDiscovery::DiscoveredAt),
			)
			.to_owned();
		manager
			.get_connection()
			.execute(manager.get_database_backend().build(&backfill))
			.await?;
		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(PlayerSetDiscovery::Table)
					.drop_column(PlayerSetDiscovery::DeliveredAt)
					.to_owned(),
			)
			.await
	}
}
