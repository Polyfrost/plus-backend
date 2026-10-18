use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// A creator code. With a coupon linked, using it also applies that coupon.
#[derive(DeriveIden)]
enum Referral {
	Table,
	/// Stored and matched upper case.
	Code,
	Name,
	DiscountId,
	Enabled,
	CreatedAt,
}

#[derive(DeriveIden)]
enum Discount {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Transaction {
	Table,
	/// No foreign key: attribution outlives a deleted code.
	ReferralCode,
}

#[derive(DeriveIden)]
enum TrackedLinks {
	Table,
	ReferralCode,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(Referral::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(Referral::Code)
							.text()
							.not_null()
							.primary_key(),
					)
					.col(ColumnDef::new(Referral::Name).text().not_null())
					.col(ColumnDef::new(Referral::DiscountId).integer().null())
					.col(
						ColumnDef::new(Referral::Enabled)
							.boolean()
							.not_null()
							.default(true),
					)
					.col(
						ColumnDef::new(Referral::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Referral::Table, Referral::DiscountId)
							.to(Discount::Table, Discount::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.add_column(ColumnDef::new(Transaction::ReferralCode).text().null())
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(TrackedLinks::Table)
					.add_column(ColumnDef::new(TrackedLinks::ReferralCode).text().null())
					.add_foreign_key(
						TableForeignKey::new()
							.from_tbl(TrackedLinks::Table)
							.from_col(TrackedLinks::ReferralCode)
							.to_tbl(Referral::Table)
							.to_col(Referral::Code)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(TrackedLinks::Table)
					.drop_column(TrackedLinks::ReferralCode)
					.to_owned(),
			)
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Transaction::Table)
					.drop_column(Transaction::ReferralCode)
					.to_owned(),
			)
			.await?;
		manager
			.drop_table(Table::drop().table(Referral::Table).to_owned())
			.await
	}
}
