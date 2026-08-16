use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
pub struct OwnershipEventKind;

#[derive(DeriveIden, EnumIter)]
pub enum OwnershipEventKindVariants {
	Granted,
	Revoked,
}

#[derive(DeriveIden)]
pub enum CosmeticOwnershipEvent {
	Table,
	Id,
	PlayerId,
	CosmeticId,
	Kind,
	Provider,
	TransactionId,
	OccurredAt,
}

#[derive(DeriveIden)]
enum AnalyticsCosmeticSnapshot {
	Table,
	WeekStart,
	CosmeticId,
	Owners,
	Equipped,
	ComputedAt,
}

#[derive(DeriveIden)]
enum AnalyticsSlotSnapshot {
	Table,
	WeekStart,
	Slot,
	EquippedPlayers,
	ComputedAt,
}

#[derive(DeriveIden)]
enum TransactionProvider {
	#[sea_orm(iden = "transaction_provider")]
	Enum,
}

#[derive(DeriveIden)]
enum BodySlot {
	#[sea_orm(iden = "body_slot")]
	Enum,
}

const OCCURRED_IDX: &str = "cosmetic_ownership_event_occurred_at_idx";
const COSMETIC_IDX: &str = "cosmetic_ownership_event_cosmetic_occurred_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(OwnershipEventKind)
					.values(OwnershipEventKindVariants::iter())
					.to_owned(),
			)
			.await?;

		// A refund deletes the player_owned_cosmetic row, so without this an
		// acquisition that was later refunded leaves no trace.
		manager
			.create_table(
				Table::create()
					.table(CosmeticOwnershipEvent::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(CosmeticOwnershipEvent::Id)
							.big_integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(CosmeticOwnershipEvent::PlayerId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(CosmeticOwnershipEvent::CosmeticId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(CosmeticOwnershipEvent::Kind)
							.custom(OwnershipEventKind)
							.not_null(),
					)
					.col(
						ColumnDef::new(CosmeticOwnershipEvent::Provider)
							.custom(TransactionProvider::Enum)
							.not_null(),
					)
					.col(
						ColumnDef::new(CosmeticOwnershipEvent::TransactionId)
							.integer()
							.null(),
					)
					.col(
						ColumnDef::new(CosmeticOwnershipEvent::OccurredAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								CosmeticOwnershipEvent::Table,
								CosmeticOwnershipEvent::PlayerId,
							)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(OCCURRED_IDX)
					.table(CosmeticOwnershipEvent::Table)
					.col(CosmeticOwnershipEvent::OccurredAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(COSMETIC_IDX)
					.table(CosmeticOwnershipEvent::Table)
					.col(CosmeticOwnershipEvent::CosmeticId)
					.col(CosmeticOwnershipEvent::OccurredAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(AnalyticsCosmeticSnapshot::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(AnalyticsCosmeticSnapshot::WeekStart)
							.date()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticSnapshot::CosmeticId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticSnapshot::Owners)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticSnapshot::Equipped)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsCosmeticSnapshot::ComputedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(AnalyticsCosmeticSnapshot::WeekStart)
							.col(AnalyticsCosmeticSnapshot::CosmeticId),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(AnalyticsSlotSnapshot::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(AnalyticsSlotSnapshot::WeekStart)
							.date()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsSlotSnapshot::Slot)
							.custom(BodySlot::Enum)
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsSlotSnapshot::EquippedPlayers)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsSlotSnapshot::ComputedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(AnalyticsSlotSnapshot::WeekStart)
							.col(AnalyticsSlotSnapshot::Slot),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(AnalyticsSlotSnapshot::Table).to_owned())
			.await?;
		manager
			.drop_table(
				Table::drop()
					.table(AnalyticsCosmeticSnapshot::Table)
					.to_owned(),
			)
			.await?;
		manager
			.drop_table(
				Table::drop()
					.table(CosmeticOwnershipEvent::Table)
					.to_owned(),
			)
			.await?;
		manager
			.drop_type(Type::drop().name(OwnershipEventKind).to_owned())
			.await
	}
}
