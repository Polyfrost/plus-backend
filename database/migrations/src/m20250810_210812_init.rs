use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable}
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// Create users table
		manager
			.create_table(
				Table::create()
					.table(User::Table)
					.if_not_exists()
					.col(ColumnDef::new(User::Id).uuid().primary_key())
					.col(
						ColumnDef::new(User::MinecraftUuid)
							.uuid()
							.unique_key()
							.not_null()
					)
					.to_owned()
			)
			.await?;

		// Create cosmetics table
		manager
			.create_type(
				Type::create()
					.as_enum(CosmeticType)
					.values(CosmeticTypeVariants::iter())
					.to_owned()
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(Cosmetic::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(Cosmetic::Id)
							.integer()
							.auto_increment()
							.primary_key()
					)
					.col(ColumnDef::new(Cosmetic::Type).custom(CosmeticType))
					.col(ColumnDef::new(Cosmetic::Path).string().not_null())
					.to_owned()
			)
			.await?;

		// Create UserCosmetic join table (for many-to-many relations)
		manager
			.create_table(
				Table::create()
					.table(UserCosmetic::Table)
					.if_not_exists()
					.col(ColumnDef::new(UserCosmetic::User).uuid().not_null())
					.col(ColumnDef::new(UserCosmetic::Cosmetic).integer().not_null())
					.col(
						ColumnDef::new(UserCosmetic::TransactionId)
							.string_len(25)
							.not_null()
					)
					.foreign_key(
						ForeignKey::create()
							.name(UserCosmeticForeignKey::User.to_string())
							.from_col(UserCosmetic::User)
							.to(User::Table, User::Id)
					)
					.foreign_key(
						ForeignKey::create()
							.name(UserCosmeticForeignKey::Cosmetic.to_string())
							.from_col(UserCosmetic::Cosmetic)
							.to(Cosmetic::Table, Cosmetic::Id)
					)
					.primary_key(
						Index::create()
							.col(UserCosmetic::User)
							.col(UserCosmetic::Cosmetic)
					)
					.to_owned()
			)
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// Drop all tables
		manager
			.drop_table(Table::drop().table(UserCosmetic::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(User::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Cosmetic::Table).to_owned())
			.await?;
		// Drop types
		manager
			.drop_type(Type::drop().name(CosmeticType).to_owned())
			.await?;

		Ok(())
	}
}

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
	MinecraftUuid
}

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	Id,
	Type,
	Path
}

#[derive(DeriveIden)]
struct CosmeticType;

#[derive(DeriveIden, EnumIter)]
pub enum CosmeticTypeVariants {
	Cape
}

#[derive(DeriveIden)]
enum UserCosmetic {
	Table,
	User,
	Cosmetic,
	TransactionId
}

#[derive(DeriveIden)]
enum UserCosmeticForeignKey {
	#[sea_orm(iden = "FK_UserCosmetic_User")]
	User,
	#[sea_orm(iden = "FK_UserCosmetic_Cosmetic")]
	Cosmetic
}
