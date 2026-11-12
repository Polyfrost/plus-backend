use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum ApiToken {
	Table,
	/// u64 permission flags, like a role's, letting a service call guarded
	/// endpoints as itself. 0 keeps the token a pure rate-limit exemption.
	Permissions,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(ApiToken::Table)
					.add_column(
						ColumnDef::new(ApiToken::Permissions)
							.big_integer()
							.not_null()
							.default(0),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(ApiToken::Table)
					.drop_column(ApiToken::Permissions)
					.to_owned(),
			)
			.await
	}
}
