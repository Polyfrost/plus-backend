use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
	ActiveOutfitId,
	OutfitsSeeded,
}

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum BodySlot {
	#[sea_orm(iden = "body_slot")]
	Enum,
}

#[derive(DeriveIden)]
enum PlayerSkin {
	Table,
	Id,
	PlayerId,
	Name,
	Model,
	Hash,
	Favorite,
	CreatedAt,
	UpdatedAt,
}

#[derive(DeriveIden)]
enum PlayerOutfit {
	Table,
	Id,
	PlayerId,
	Name,
	Favorite,
	SkinId,
	MojangCapeId,
	ParticleColor,
	Blank,
	CreatedAt,
	UpdatedAt,
}

#[derive(DeriveIden)]
enum PlayerOutfitCosmetic {
	Table,
	OutfitId,
	Slot,
	CosmeticId,
}

const SKIN_PLAYER_HASH_IDX: &str = "player_skin_player_id_hash_key";
const OUTFIT_PLAYER_IDX: &str = "player_outfit_player_id_idx";
const USER_ACTIVE_OUTFIT_FK: &str = "user_active_outfit_id_fkey";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(PlayerSkin::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerSkin::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(PlayerSkin::PlayerId).integer().not_null())
					.col(ColumnDef::new(PlayerSkin::Name).text().not_null())
					.col(ColumnDef::new(PlayerSkin::Model).text().not_null())
					.col(ColumnDef::new(PlayerSkin::Hash).text().not_null())
					.col(
						ColumnDef::new(PlayerSkin::Favorite)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(
						ColumnDef::new(PlayerSkin::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PlayerSkin::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerSkin::Table, PlayerSkin::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(SKIN_PLAYER_HASH_IDX)
					.table(PlayerSkin::Table)
					.col(PlayerSkin::PlayerId)
					.col(PlayerSkin::Hash)
					.unique()
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PlayerOutfit::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerOutfit::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(PlayerOutfit::PlayerId).integer().not_null())
					.col(ColumnDef::new(PlayerOutfit::Name).text().not_null())
					.col(
						ColumnDef::new(PlayerOutfit::Favorite)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(ColumnDef::new(PlayerOutfit::SkinId).integer().null())
					.col(ColumnDef::new(PlayerOutfit::MojangCapeId).text().null())
					.col(ColumnDef::new(PlayerOutfit::ParticleColor).integer().null())
					.col(
						ColumnDef::new(PlayerOutfit::Blank)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(
						ColumnDef::new(PlayerOutfit::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PlayerOutfit::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerOutfit::Table, PlayerOutfit::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlayerOutfit::Table, PlayerOutfit::SkinId)
							.to(PlayerSkin::Table, PlayerSkin::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(OUTFIT_PLAYER_IDX)
					.table(PlayerOutfit::Table)
					.col(PlayerOutfit::PlayerId)
					.if_not_exists()
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PlayerOutfitCosmetic::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlayerOutfitCosmetic::OutfitId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(PlayerOutfitCosmetic::Slot)
							.custom(BodySlot::Enum)
							.not_null(),
					)
					.col(
						ColumnDef::new(PlayerOutfitCosmetic::CosmeticId)
							.integer()
							.not_null(),
					)
					.primary_key(
						Index::create()
							.col(PlayerOutfitCosmetic::OutfitId)
							.col(PlayerOutfitCosmetic::Slot),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								PlayerOutfitCosmetic::Table,
								PlayerOutfitCosmetic::OutfitId,
							)
							.to(PlayerOutfit::Table, PlayerOutfit::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								PlayerOutfitCosmetic::Table,
								PlayerOutfitCosmetic::CosmeticId,
							)
							.to(Cosmetic::Table, Cosmetic::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(User::Table)
					.add_column(ColumnDef::new(User::ActiveOutfitId).integer().null())
					.add_foreign_key(
						TableForeignKey::new()
							.name(USER_ACTIVE_OUTFIT_FK)
							.from_tbl(User::Table)
							.from_col(User::ActiveOutfitId)
							.to_tbl(PlayerOutfit::Table)
							.to_col(PlayerOutfit::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.add_column(
						ColumnDef::new(User::OutfitsSeeded)
							.boolean()
							.not_null()
							.default(false),
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
					.drop_column(User::ActiveOutfitId)
					.drop_column(User::OutfitsSeeded)
					.to_owned(),
			)
			.await?;
		manager
			.drop_table(Table::drop().table(PlayerOutfitCosmetic::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(PlayerOutfit::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(PlayerSkin::Table).to_owned())
			.await
	}
}
