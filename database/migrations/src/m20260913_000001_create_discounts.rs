use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// One table for both, separated by `code`: null auto-applies (a sale), set
/// has to be typed in (a coupon). Everything else — the amount, the window,
/// what it covers — is the same question either way.
#[derive(DeriveIden)]
enum Discount {
	Table,
	Id,
	Name,
	Description,
	Code,
	PercentOff,
	AmountOffMinor,
	/// Required alongside `amount_off_minor`: a fixed amount only means
	/// something in the currency it was written in.
	Currency,
	StartsAt,
	EndsAt,
	Enabled,
	AppliesToAll,
	MinSubtotalMinor,
	MaxRedemptions,
	MaxPerPlayer,
	Redemptions,
	/// The sale or coupon mirroring this on PayNow.
	PaynowId,
	CreatedAt,
}

#[derive(DeriveIden)]
enum DiscountTarget {
	Table,
	Id,
	DiscountId,
	CollectionId,
	TagId,
	CosmeticId,
	CosmeticGroupId,
	BundleId,
}

#[derive(DeriveIden)]
enum DiscountRedemption {
	Table,
	Id,
	DiscountId,
	PlayerId,
	TransactionId,
	RedeemedAt,
}

#[derive(DeriveIden)]
enum Collections {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Tags {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Cosmetic {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum CosmeticGroup {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Bundles {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum Transaction {
	Table,
	Id,
}

const CODE_IDX: &str = "discount_code_idx";
const ACTIVE_IDX: &str = "discount_active_idx";
const TARGET_DISCOUNT_IDX: &str = "discount_target_discount_idx";
const REDEMPTION_PLAYER_IDX: &str = "discount_redemption_player_idx";

/// Exactly one of the two amounts, a percentage that is really a percentage,
/// and a currency whenever the amount needs one.
const AMOUNT_CHECK: &str = r#"(
	(percent_off IS NOT NULL AND amount_off_minor IS NULL AND percent_off BETWEEN 1 AND 100)
	OR (amount_off_minor IS NOT NULL AND percent_off IS NULL AND amount_off_minor > 0 AND currency IS NOT NULL)
)"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(Discount::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(Discount::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(Discount::Name).text().not_null())
					.col(ColumnDef::new(Discount::Description).text().null())
					.col(ColumnDef::new(Discount::Code).text().null())
					.col(ColumnDef::new(Discount::PercentOff).integer().null())
					.col(ColumnDef::new(Discount::AmountOffMinor).big_integer().null())
					.col(ColumnDef::new(Discount::Currency).text().null())
					.col(
						ColumnDef::new(Discount::StartsAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(Discount::EndsAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(Discount::Enabled)
							.boolean()
							.not_null()
							.default(true),
					)
					.col(
						ColumnDef::new(Discount::AppliesToAll)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(
						ColumnDef::new(Discount::MinSubtotalMinor)
							.big_integer()
							.null(),
					)
					.col(ColumnDef::new(Discount::MaxRedemptions).integer().null())
					.col(ColumnDef::new(Discount::MaxPerPlayer).integer().null())
					.col(
						ColumnDef::new(Discount::Redemptions)
							.integer()
							.not_null()
							.default(0),
					)
					.col(ColumnDef::new(Discount::PaynowId).text().null())
					.col(
						ColumnDef::new(Discount::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.check(Expr::cust(AMOUNT_CHECK))
					.to_owned(),
			)
			.await?;

		// Codes are upper cased before they are written or looked up, so a
		// plain unique index is already case insensitive.
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(CODE_IDX)
					.table(Discount::Table)
					.col(Discount::Code)
					.unique()
					.and_where(Expr::col(Discount::Code).is_not_null())
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(ACTIVE_IDX)
					.table(Discount::Table)
					.col(Discount::Enabled)
					.col(Discount::StartsAt)
					.col(Discount::EndsAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(DiscountTarget::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(DiscountTarget::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(DiscountTarget::DiscountId)
							.integer()
							.not_null(),
					)
					.col(ColumnDef::new(DiscountTarget::CollectionId).integer().null())
					.col(ColumnDef::new(DiscountTarget::TagId).integer().null())
					.col(ColumnDef::new(DiscountTarget::CosmeticId).integer().null())
					.col(
						ColumnDef::new(DiscountTarget::CosmeticGroupId)
							.integer()
							.null(),
					)
					.col(ColumnDef::new(DiscountTarget::BundleId).integer().null())
					.foreign_key(
						ForeignKey::create()
							.from(DiscountTarget::Table, DiscountTarget::DiscountId)
							.to(Discount::Table, Discount::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DiscountTarget::Table, DiscountTarget::CollectionId)
							.to(Collections::Table, Collections::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DiscountTarget::Table, DiscountTarget::TagId)
							.to(Tags::Table, Tags::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DiscountTarget::Table, DiscountTarget::CosmeticId)
							.to(Cosmetic::Table, Cosmetic::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DiscountTarget::Table, DiscountTarget::CosmeticGroupId)
							.to(CosmeticGroup::Table, CosmeticGroup::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DiscountTarget::Table, DiscountTarget::BundleId)
							.to(Bundles::Table, Bundles::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(TARGET_DISCOUNT_IDX)
					.table(DiscountTarget::Table)
					.col(DiscountTarget::DiscountId)
					.to_owned(),
			)
			.await?;

		// Per-player limits need the trail, and so does anyone asking which
		// code actually sold anything.
		manager
			.create_table(
				Table::create()
					.table(DiscountRedemption::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(DiscountRedemption::Id)
							.big_integer()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(DiscountRedemption::DiscountId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(DiscountRedemption::PlayerId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(DiscountRedemption::TransactionId)
							.integer()
							.null(),
					)
					.col(
						ColumnDef::new(DiscountRedemption::RedeemedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								DiscountRedemption::Table,
								DiscountRedemption::DiscountId,
							)
							.to(Discount::Table, Discount::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(DiscountRedemption::Table, DiscountRedemption::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								DiscountRedemption::Table,
								DiscountRedemption::TransactionId,
							)
							.to(Transaction::Table, Transaction::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(REDEMPTION_PLAYER_IDX)
					.table(DiscountRedemption::Table)
					.col(DiscountRedemption::DiscountId)
					.col(DiscountRedemption::PlayerId)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(DiscountRedemption::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(DiscountTarget::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Discount::Table).to_owned())
			.await
	}
}
