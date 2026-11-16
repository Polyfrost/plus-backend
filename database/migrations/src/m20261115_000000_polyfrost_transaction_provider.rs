use sea_orm_migration::prelude::{extension::postgres::Type, *};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum TransactionProvider {
	#[sea_orm(iden = "transaction_provider")]
	Enum,
	#[sea_orm(iden = "polyfrost")]
	Polyfrost,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_type(
				Type::alter()
					.name(TransactionProvider::Enum)
					.add_value(TransactionProvider::Polyfrost)
					.if_not_exists(),
			)
			.await
	}

	// Postgres cannot drop an enum value.
	async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
		Ok(())
	}
}
