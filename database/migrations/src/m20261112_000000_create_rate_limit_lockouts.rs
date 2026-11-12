use sea_orm_migration::prelude::*;

/// Each time a rate limiter locked a key out, kept for a week to tune the
/// limits against.
#[derive(DeriveIden)]
pub enum RateLimitLockout {
	Table,
	Id,
	Limiter,
	/// The player id, address or connection that was locked out.
	Key,
	TimeoutSeconds,
	/// The limit and window in force at the time.
	RequestLimit,
	WindowSeconds,
	CreatedAt,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(RateLimitLockout::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(RateLimitLockout::Id)
							.big_integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(RateLimitLockout::Limiter).text().not_null())
					.col(ColumnDef::new(RateLimitLockout::Key).text().not_null())
					.col(
						ColumnDef::new(RateLimitLockout::TimeoutSeconds)
							.big_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(RateLimitLockout::RequestLimit)
							.big_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(RateLimitLockout::WindowSeconds)
							.big_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(RateLimitLockout::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("idx_rate_limit_lockout_created_at")
					.table(RateLimitLockout::Table)
					.col(RateLimitLockout::CreatedAt)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(RateLimitLockout::Table).to_owned())
			.await
	}
}
