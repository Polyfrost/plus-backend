use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
pub struct SharedEmotePrivacy;

#[derive(DeriveIden, EnumIter)]
pub enum SharedEmotePrivacyVariants {
	Everyone,
	Friends,
	Nobody,
}

#[derive(DeriveIden)]
enum User {
	Table,
	SharedEmotePrivacy,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(SharedEmotePrivacy)
					.values(SharedEmotePrivacyVariants::iter())
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(User::Table)
					.add_column(
						ColumnDef::new(User::SharedEmotePrivacy)
							.custom(SharedEmotePrivacy)
							.not_null()
							.default("everyone"),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(User::Table)
					.drop_column(User::SharedEmotePrivacy)
					.to_owned(),
			)
			.await?;
		manager
			.drop_type(Type::drop().name(SharedEmotePrivacy).to_owned())
			.await
	}
}
