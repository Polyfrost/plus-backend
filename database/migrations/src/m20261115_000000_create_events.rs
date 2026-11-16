use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Event {
	Table,
	Id,
	Name,
	/// Kept equal to the first slot opening and the last one closing.
	StartsAt,
	EndsAt,
	/// The kind and its settings, rewards included.
	Config,
	CreatedAt,
}

/// No foreign key to the event's slots, so editing an event keeps who
/// already claimed what.
#[derive(DeriveIden)]
enum EventClaim {
	Table,
	EventId,
	UserId,
	Slot,
	/// What the player received, e.g. a generated coupon code.
	Granted,
	ClaimedAt,
}

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(Event::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(Event::Id)
							.integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(Event::Name).text().not_null())
					.col(
						ColumnDef::new(Event::StartsAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.col(
						ColumnDef::new(Event::EndsAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.col(ColumnDef::new(Event::Config).json_binary().not_null())
					.col(
						ColumnDef::new(Event::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(EventClaim::Table)
					.if_not_exists()
					.col(ColumnDef::new(EventClaim::EventId).integer().not_null())
					.col(ColumnDef::new(EventClaim::UserId).integer().not_null())
					.col(ColumnDef::new(EventClaim::Slot).text().not_null())
					.col(ColumnDef::new(EventClaim::Granted).json_binary().not_null())
					.col(
						ColumnDef::new(EventClaim::ClaimedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(EventClaim::EventId)
							.col(EventClaim::UserId)
							.col(EventClaim::Slot),
					)
					.foreign_key(
						ForeignKey::create()
							.from(EventClaim::Table, EventClaim::EventId)
							.to(Event::Table, Event::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(EventClaim::Table, EventClaim::UserId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(EventClaim::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Event::Table).to_owned())
			.await
	}
}
