use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

use crate::m20260731_000000_create_social_tables::SessionInvites;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PENDING_INVITES_INDEX: &str = "session_invites_pending_session_id_idx";

#[derive(DeriveIden)]
struct GameLoader;

#[derive(DeriveIden, EnumIter)]
enum GameLoaderVariants {
	Fabric,
	Forge,
	Neoforge,
	Quilt,
	Ornithe,
}

#[derive(DeriveIden)]
enum GameSessions {
	Table,
	MinecraftVersion,
	Loader,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(GameLoader)
					.values(GameLoaderVariants::iter())
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(GameSessions::Table)
					.add_column(ColumnDef::new(GameSessions::MinecraftVersion).text().null())
					.add_column(
						ColumnDef::new(GameSessions::Loader)
							.custom(GameLoader)
							.null(),
					)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(PENDING_INVITES_INDEX)
					.table(SessionInvites::Table)
					.col(SessionInvites::SessionId)
					.and_where(Expr::col(SessionInvites::Status).eq("pending"))
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_index(Index::drop().if_exists().name(PENDING_INVITES_INDEX).to_owned())
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(GameSessions::Table)
					.drop_column(GameSessions::MinecraftVersion)
					.drop_column(GameSessions::Loader)
					.to_owned(),
			)
			.await?;
		manager
			.drop_type(Type::drop().name(GameLoader).to_owned())
			.await
	}
}
