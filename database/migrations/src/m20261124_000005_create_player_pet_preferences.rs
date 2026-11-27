use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum PlayerPetPreferences {
	Table,
	PlayerId,
	Interactions,
	UpdatedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PlayerPetPreferences::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerPetPreferences::PlayerId)
							.integer()
							.not_null()
							.primary_key(),
					)
					.col(
						ColumnDef::new(PlayerPetPreferences::Interactions)
							.boolean()
							.not_null()
							.default(true),
					)
					.col(
						ColumnDef::new(PlayerPetPreferences::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								PlayerPetPreferences::Table,
								PlayerPetPreferences::PlayerId,
							)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(PlayerPetPreferences::Table).to_owned())
			.await
	}
}
