use sea_orm_migration::prelude::*;

/// A token issued to a first-party service so its traffic can skip the rate
/// limiter. Only the hash of the secret is stored: the plaintext exists once,
/// in the response that created it.
#[derive(DeriveIden)]
pub enum ApiToken {
	Table,
	Id,
	Label,
	TokenHash,
	/// Request path prefixes this token lifts the rate limit on. `/` exempts every endpoint.
	ExemptPrefixes,
	CreatedAt,
	RevokedAt,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(ApiToken::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(ApiToken::Id)
							.integer()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(ApiToken::Label).text().not_null())
					.col(
						ColumnDef::new(ApiToken::TokenHash)
							.text()
							.not_null()
							.unique_key(),
					)
					.col(
						ColumnDef::new(ApiToken::ExemptPrefixes)
							.array(ColumnType::Text)
							.not_null(),
					)
					.col(
						ColumnDef::new(ApiToken::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(ColumnDef::new(ApiToken::RevokedAt).timestamp_with_time_zone())
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(ApiToken::Table).to_owned())
			.await
	}
}
