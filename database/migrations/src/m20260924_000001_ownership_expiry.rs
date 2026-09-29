use sea_orm_migration::prelude::{extension::postgres::Type, *};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PlayerOwnedCosmetic {
	Table,
	PlayerId,
	CosmeticId,
	AcquiredVia,
	TransactionId,
	TransactionLineId,
	AcquiredAt,
	ExpiresAt,
}

#[derive(DeriveIden)]
enum CosmeticOwnershipEvent {
	Table,
	PlayerId,
	CosmeticId,
	Kind,
	Provider,
	TransactionId,
	TransactionLineId,
	OccurredAt,
	ExpiresAt,
}

/// One row per purchase or grant of a cosmetic. `player_owned_cosmetic` is
/// what these add up to, so a refund can take back exactly one of them.
#[derive(DeriveIden)]
enum OwnershipGrant {
	Table,
	Id,
	PlayerId,
	CosmeticId,
	Provider,
	TransactionId,
	TransactionLineId,
	/// Null is for good.
	Days,
	GrantedAt,
	/// False once refunded or charged back.
	Active,
}

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Transaction {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum TransactionLine {
	Table,
	Id,
	Status,
}

#[derive(DeriveIden)]
struct OwnershipEventKind;

#[derive(DeriveIden)]
struct TransactionProvider;

#[derive(DeriveIden)]
struct TransactionStatus;

#[derive(DeriveIden)]
struct Expired;

const EXPIRES_IDX: &str = "player_owned_cosmetic_expires_idx";
const GRANT_OWNER_IDX: &str = "ownership_grant_owner_idx";
const GRANT_LINE_IDX: &str = "ownership_grant_line_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// Null is owned for good.
		manager
			.alter_table(
				Table::alter()
					.table(PlayerOwnedCosmetic::Table)
					.add_column(
						ColumnDef::new(PlayerOwnedCosmetic::ExpiresAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(EXPIRES_IDX)
					.table(PlayerOwnedCosmetic::Table)
					.col(PlayerOwnedCosmetic::ExpiresAt)
					.and_where(Expr::col(PlayerOwnedCosmetic::ExpiresAt).is_not_null())
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(CosmeticOwnershipEvent::Table)
					.add_column(
						ColumnDef::new(CosmeticOwnershipEvent::ExpiresAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_type(
				Type::alter()
					.name(OwnershipEventKind)
					.add_value(Expired)
					.if_not_exists(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(OwnershipGrant::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(OwnershipGrant::Id)
							.big_integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(OwnershipGrant::PlayerId).integer().not_null())
					.col(ColumnDef::new(OwnershipGrant::CosmeticId).integer().not_null())
					.col(
						ColumnDef::new(OwnershipGrant::Provider)
							.custom(TransactionProvider)
							.not_null(),
					)
					.col(ColumnDef::new(OwnershipGrant::TransactionId).integer().null())
					.col(
						ColumnDef::new(OwnershipGrant::TransactionLineId)
							.big_integer()
							.null(),
					)
					.col(ColumnDef::new(OwnershipGrant::Days).integer().null())
					.col(
						ColumnDef::new(OwnershipGrant::GrantedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(OwnershipGrant::Active)
							.boolean()
							.not_null()
							.default(true),
					)
					.foreign_key(
						ForeignKey::create()
							.from(OwnershipGrant::Table, OwnershipGrant::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(OwnershipGrant::Table, OwnershipGrant::CosmeticId)
							.to(Cosmetic::Table, Cosmetic::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(OwnershipGrant::Table, OwnershipGrant::TransactionId)
							.to(Transaction::Table, Transaction::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.foreign_key(
						ForeignKey::create()
							.from(OwnershipGrant::Table, OwnershipGrant::TransactionLineId)
							.to(TransactionLine::Table, TransactionLine::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(GRANT_OWNER_IDX)
					.table(OwnershipGrant::Table)
					.col(OwnershipGrant::PlayerId)
					.col(OwnershipGrant::CosmeticId)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(GRANT_LINE_IDX)
					.table(OwnershipGrant::Table)
					.col(OwnershipGrant::TransactionLineId)
					.to_owned(),
			)
			.await?;

		let grant_columns = || [
			OwnershipGrant::PlayerId,
			OwnershipGrant::CosmeticId,
			OwnershipGrant::Provider,
			OwnershipGrant::TransactionId,
			OwnershipGrant::TransactionLineId,
			OwnershipGrant::Days,
			OwnershipGrant::GrantedAt,
			OwnershipGrant::Active,
		];

		// Everything owned today is owned for good.
		manager
			.exec_stmt(
				Query::insert()
					.into_table(OwnershipGrant::Table)
					.columns(grant_columns())
					.select_from(
						Query::select()
							.columns([
								PlayerOwnedCosmetic::PlayerId,
								PlayerOwnedCosmetic::CosmeticId,
								PlayerOwnedCosmetic::AcquiredVia,
								PlayerOwnedCosmetic::TransactionId,
								PlayerOwnedCosmetic::TransactionLineId,
							])
							.expr(Expr::val(Option::<i32>::None))
							.column(PlayerOwnedCosmetic::AcquiredAt)
							.expr(Expr::val(true))
							.from(PlayerOwnedCosmetic::Table)
							.to_owned(),
					)
					.map_err(|error| DbErr::Migration(error.to_string()))?
					.to_owned(),
			)
			.await?;

		// A dispute still open was revoked from `player_owned_cosmetic`, so
		// its grants come from the event trail, inactive until it is won.
		manager
			.exec_stmt(
				Query::insert()
					.into_table(OwnershipGrant::Table)
					.columns(grant_columns())
					.select_from(
						Query::select()
							.columns([
								(CosmeticOwnershipEvent::Table, CosmeticOwnershipEvent::PlayerId),
								(
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::CosmeticId,
								),
								(CosmeticOwnershipEvent::Table, CosmeticOwnershipEvent::Provider),
								(
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::TransactionId,
								),
								(
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::TransactionLineId,
								),
							])
							.expr(Expr::val(Option::<i32>::None))
							.expr(
								Expr::col((
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::OccurredAt,
								))
								.min(),
							)
							.expr(Expr::val(false))
							.from(CosmeticOwnershipEvent::Table)
							.inner_join(
								TransactionLine::Table,
								Expr::col((TransactionLine::Table, TransactionLine::Id)).equals((
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::TransactionLineId,
								)),
							)
							.and_where(
								Expr::col((CosmeticOwnershipEvent::Table, CosmeticOwnershipEvent::Kind))
									.eq(Expr::val("granted").as_enum(OwnershipEventKind)),
							)
							.and_where(
								Expr::col((TransactionLine::Table, TransactionLine::Status))
									.eq(Expr::val("chargeback").as_enum(TransactionStatus)),
							)
							.group_by_columns([
								(CosmeticOwnershipEvent::Table, CosmeticOwnershipEvent::PlayerId),
								(
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::CosmeticId,
								),
								(CosmeticOwnershipEvent::Table, CosmeticOwnershipEvent::Provider),
								(
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::TransactionId,
								),
								(
									CosmeticOwnershipEvent::Table,
									CosmeticOwnershipEvent::TransactionLineId,
								),
							])
							.to_owned(),
					)
					.map_err(|error| DbErr::Migration(error.to_string()))?
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(OwnershipGrant::Table).to_owned())
			.await?;

		// Postgres cannot drop an enum value, so `expired` stays.
		manager
			.alter_table(
				Table::alter()
					.table(CosmeticOwnershipEvent::Table)
					.drop_column(CosmeticOwnershipEvent::ExpiresAt)
					.to_owned(),
			)
			.await?;
		manager
			.drop_index(
				Index::drop()
					.name(EXPIRES_IDX)
					.table(PlayerOwnedCosmetic::Table)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(PlayerOwnedCosmetic::Table)
					.drop_column(PlayerOwnedCosmetic::ExpiresAt)
					.to_owned(),
			)
			.await
	}
}
