use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerCosmeticColors {
	Table,
	Active,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(PlayerCosmeticColors::Table)
					.add_column(
						ColumnDef::new(PlayerCosmeticColors::Active)
							.boolean()
							.not_null()
							.default(true),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		let inactive = Query::select()
			.expr_as(
				Expr::exists(
					Query::select()
						.expr(Expr::val(1))
						.from(PlayerCosmeticColors::Table)
						.and_where(Expr::col(PlayerCosmeticColors::Active).eq(false))
						.to_owned(),
				),
				Alias::new("used"),
			)
			.to_owned();
		let used = manager
			.get_connection()
			.query_one(manager.get_database_backend().build(&inactive))
			.await?
			.map(|row| row.try_get::<bool>("", "used"))
			.transpose()?
			.unwrap_or(false);
		if used {
			return Err(DbErr::Migration(
				concat!(
					"Can't roll back while player_cosmetic_colors still has inactive rows. ",
					"Without the active column they would all become active, so delete them first."
				)
				.to_owned(),
			));
		}

		manager
			.alter_table(
				Table::alter()
					.table(PlayerCosmeticColors::Table)
					.drop_column(PlayerCosmeticColors::Active)
					.to_owned(),
			)
			.await
	}
}
