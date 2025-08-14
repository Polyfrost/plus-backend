use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable}
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// Create players table
		manager
			.create_table(
				Table::create()
					.table(Player::Table)
					.if_not_exists()
					.col(ColumnDef::new(Player::MinecraftUuid).uuid().primary_key())
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

		// Create PlayerCosmetic table (for many-to-many relations)
		manager
			.create_table(
				Table::create()
					.table(PlayerCosmetic::Table)
					.if_not_exists()
					.col(ColumnDef::new(PlayerCosmetic::Player).uuid().not_null())
					.col(
						ColumnDef::new(PlayerCosmetic::Cosmetic)
							.integer()
							.not_null()
					)
					.col(
						ColumnDef::new(PlayerCosmetic::TransactionId)
							.string_len(25)
							.not_null()
					)
					.foreign_key(
						ForeignKey::create()
							.name(PlayerCosmeticForeignKey::Player.to_string())
							.from_col(PlayerCosmetic::Player)
							.to(Player::Table, Player::MinecraftUuid)
					)
					.foreign_key(
						ForeignKey::create()
							.name(PlayerCosmeticForeignKey::Cosmetic.to_string())
							.from_col(PlayerCosmetic::Cosmetic)
							.to(Cosmetic::Table, Cosmetic::Id)
					)
					.primary_key(
						Index::create()
							.col(PlayerCosmetic::Player)
							.col(PlayerCosmetic::Cosmetic)
					)
					.to_owned()
			)
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// Drop all tables
		manager
			.drop_table(Table::drop().table(PlayerCosmetic::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Player::Table).to_owned())
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
enum Player {
	Table,
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
enum PlayerCosmetic {
	Table,
	Player,
	Cosmetic,
	TransactionId
}

#[derive(DeriveIden)]
enum PlayerCosmeticForeignKey {
	#[sea_orm(iden = "FK_PlayerCosmetic_Player")]
	Player,
	#[sea_orm(iden = "FK_PlayerCosmetic_Cosmetic")]
	Cosmetic
}
