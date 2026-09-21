use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AnalyticsClientDaily {
	Table,
	Country,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsClientDaily::Table)
					.add_column(
						ColumnDef::new(AnalyticsClientDaily::Country)
							.char_len(2)
							.not_null()
							.default(""),
					)
					.to_owned(),
			)
			.await?;

		// sea-query's alter_table cannot swap a primary key.
		manager
			.get_connection()
			.execute_unprepared(
				r"
				ALTER TABLE analytics_client_daily
					DROP CONSTRAINT analytics_client_daily_pkey,
					ADD PRIMARY KEY (day, client_version, minecraft_version, loader, os, country);
				",
			)
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.get_connection()
			.execute_unprepared(
				r"
				DELETE FROM analytics_client_daily a
					USING analytics_client_daily b
					WHERE a.country > b.country
						AND a.day = b.day
						AND a.client_version = b.client_version
						AND a.minecraft_version = b.minecraft_version
						AND a.loader = b.loader
						AND a.os = b.os;

				ALTER TABLE analytics_client_daily
					DROP CONSTRAINT analytics_client_daily_pkey,
					ADD PRIMARY KEY (day, client_version, minecraft_version, loader, os);
				",
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(AnalyticsClientDaily::Table)
					.drop_column(AnalyticsClientDaily::Country)
					.to_owned(),
			)
			.await
	}
}
