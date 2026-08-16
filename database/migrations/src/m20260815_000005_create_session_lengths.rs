use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AnalyticsSessionLengthDaily {
	Table,
	Day,
	Bucket,
	Sessions,
	ComputedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// A histogram rather than raw rows, so a percentile is a sum of eight
		// buckets a day instead of a scan over every session.
		manager
			.create_table(
				Table::create()
					.table(AnalyticsSessionLengthDaily::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(AnalyticsSessionLengthDaily::Day)
							.date()
							.not_null(),
					)
					// Index into the bucket bounds the rollup job owns.
					.col(
						ColumnDef::new(AnalyticsSessionLengthDaily::Bucket)
							.small_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(AnalyticsSessionLengthDaily::Sessions)
							.integer()
							.not_null()
							.default(0),
					)
					.col(
						ColumnDef::new(AnalyticsSessionLengthDaily::ComputedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(AnalyticsSessionLengthDaily::Day)
							.col(AnalyticsSessionLengthDaily::Bucket),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(
				Table::drop()
					.table(AnalyticsSessionLengthDaily::Table)
					.to_owned(),
			)
			.await
	}
}
