use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerCosmeticColors {
	Table,
	Color,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(PlayerCosmeticColors::Table)
					.modify_column(ColumnDef::new(PlayerCosmeticColors::Color).integer().null())
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		let cleared = Query::select()
			.expr_as(
				Expr::exists(
					Query::select()
						.expr(Expr::val(1))
						.from(PlayerCosmeticColors::Table)
						.and_where(Expr::col(PlayerCosmeticColors::Color).is_null())
						.to_owned(),
				),
				Alias::new("used"),
			)
			.to_owned();
		let used = manager
			.get_connection()
			.query_one(manager.get_database_backend().build(&cleared))
			.await?
			.map(|row| row.try_get::<bool>("", "used"))
			.transpose()?
			.unwrap_or(false);
		if used {
			return Err(DbErr::Migration(
				concat!(
					"Can't roll back while player_cosmetic_colors still has rows with no color. ",
					"They record auras reset to their design colour, so move or delete them first."
				)
				.to_owned(),
			));
		}

		manager
			.alter_table(
				Table::alter()
					.table(PlayerCosmeticColors::Table)
					.modify_column(
						ColumnDef::new(PlayerCosmeticColors::Color)
							.integer()
							.not_null(),
					)
					.to_owned(),
			)
			.await
	}
}
