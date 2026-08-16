use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
pub struct SessionEndReason;

#[derive(DeriveIden, EnumIter)]
pub enum SessionEndReasonVariants {
	/// The player's last websocket connection closed.
	Disconnect,
	/// Left open by a process that stopped; closed at the next startup.
	Reaped,
	/// Closed deliberately as the process shut down.
	Shutdown,
}

#[derive(DeriveIden)]
pub enum PlaySession {
	Table,
	Id,
	PlayerId,
	StartedAt,
	LastHeartbeatAt,
	EndedAt,
	EndReason,
}

#[derive(DeriveIden)]
pub enum AnalyticsHourly {
	Table,
	HourStart,
	Dow,
	HourOfDay,
	PlaytimeSeconds,
	ActivePlayers,
	SessionsStarted,
	PeakConcurrent,
	ComputedAt,
}

const STARTED_AT_IDX: &str = "play_session_started_at_idx";
const PLAYER_STARTED_IDX: &str = "play_session_player_started_idx";
const OPEN_IDX: &str = "play_session_open_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(SessionEndReason)
					.values(SessionEndReasonVariants::iter())
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PlaySession::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PlaySession::Id)
							.big_integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(PlaySession::PlayerId).integer().not_null())
					.col(
						ColumnDef::new(PlaySession::StartedAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.col(
						ColumnDef::new(PlaySession::LastHeartbeatAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.col(
						ColumnDef::new(PlaySession::EndedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(PlaySession::EndReason)
							.custom(SessionEndReason)
							.null(),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PlaySession::Table, PlaySession::PlayerId)
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
					.name(STARTED_AT_IDX)
					.table(PlaySession::Table)
					.col(PlaySession::StartedAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(PLAYER_STARTED_IDX)
					.table(PlaySession::Table)
					.col(PlaySession::PlayerId)
					.col(PlaySession::StartedAt)
					.to_owned(),
			)
			.await?;

		// Only open sessions are ever looked up by player, and the reaper scans
		// exactly this set.
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(OPEN_IDX)
					.table(PlaySession::Table)
					.col(PlaySession::PlayerId)
					.and_where(Expr::col(PlaySession::EndedAt).is_null())
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(AnalyticsHourly::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(AnalyticsHourly::HourStart)
							.timestamp_with_time_zone()
							.not_null()
							.primary_key(),
					)
					// Denormalised at write time: EXTRACT on a timestamptz
					// depends on the session TimeZone.
					.col(
						ColumnDef::new(AnalyticsHourly::Dow)
							.small_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsHourly::HourOfDay)
							.small_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsHourly::PlaytimeSeconds)
							.big_integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsHourly::ActivePlayers)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsHourly::SessionsStarted)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsHourly::PeakConcurrent)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsHourly::ComputedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(AnalyticsHourly::Table).to_owned())
			.await?;

		manager
			.drop_table(Table::drop().table(PlaySession::Table).to_owned())
			.await?;

		manager
			.drop_type(Type::drop().name(SessionEndReason).to_owned())
			.await
	}
}
