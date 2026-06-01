use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		let db = manager.get_connection();

		db.execute_unprepared(
			r#"
			ALTER TABLE cosmetic ALTER COLUMN type TYPE TEXT USING type::text;
			DROP TYPE cosmetic_type;
			"#,
		)
		.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		let db = manager.get_connection();

		db.execute_unprepared(
			r#"
			DO $$ BEGIN
				CREATE TYPE cosmetic_type AS ENUM ('cape', 'emote');
			EXCEPTION WHEN duplicate_object THEN NULL;
			END $$;

			ALTER TABLE cosmetic
				ALTER COLUMN type TYPE cosmetic_type USING type::cosmetic_type;
			"#,
		)
		.await?;

		Ok(())
	}
}
