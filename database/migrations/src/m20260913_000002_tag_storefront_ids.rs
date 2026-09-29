use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Tags {
	Table,
	PaynowTagId,
}

const PAYNOW_TAG_IDX: &str = "tags_paynow_tag_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// The same role `store_product_id` plays for a cosmetic: the handle on
		// the storefront copy, so a sale can be scoped to a tag there rather
		// than to every product it covers.
		manager
			.alter_table(
				Table::alter()
					.table(Tags::Table)
					.add_column(ColumnDef::new(Tags::PaynowTagId).text().null())
					.to_owned(),
			)
			.await?;

		// One local tag per storefront tag; a duplicate would mean two rows
		// fighting over the same remote name.
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(PAYNOW_TAG_IDX)
					.table(Tags::Table)
					.col(Tags::PaynowTagId)
					.unique()
					.and_where(Expr::col(Tags::PaynowTagId).is_not_null())
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_index(Index::drop().name(PAYNOW_TAG_IDX).table(Tags::Table).to_owned())
			.await?;
		manager
			.alter_table(
				Table::alter()
					.table(Tags::Table)
					.drop_column(Tags::PaynowTagId)
					.to_owned(),
			)
			.await
	}
}
