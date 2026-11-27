use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
pub struct PetInteractions;

#[derive(DeriveIden, EnumIter)]
pub enum PetInteractionsVariants {
	Everyone,
	Friends,
	Off,
}

#[derive(DeriveIden)]
enum PlayerPetPreferences {
	Table,
	Interactions,
	Play,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(PetInteractions)
					.values(PetInteractionsVariants::iter())
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(PlayerPetPreferences::Table)
					.add_column(
						ColumnDef::new(PlayerPetPreferences::Play)
							.custom(PetInteractions)
							.null(),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.exec_stmt(
				Query::update()
					.table(PlayerPetPreferences::Table)
					.value(PlayerPetPreferences::Interactions, false)
					.and_where(
						Expr::col(PlayerPetPreferences::Play)
							.cast_as(Alias::new("text"))
							.eq("friends"),
					)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(PlayerPetPreferences::Table)
					.drop_column(PlayerPetPreferences::Play)
					.to_owned(),
			)
			.await?;
		manager
			.drop_type(Type::drop().name(PetInteractions).to_owned())
			.await
	}
}
