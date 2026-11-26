use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum PartyMembers {
	Table,
	SoundboardMuted,
}

fn add_column() -> TableAlterStatement {
	Table::alter()
		.table(PartyMembers::Table)
		.add_column(
			ColumnDef::new(PartyMembers::SoundboardMuted)
				.boolean()
				.not_null()
				.default(false),
		)
		.to_owned()
}

fn drop_column() -> TableAlterStatement {
	Table::alter()
		.table(PartyMembers::Table)
		.drop_column(PartyMembers::SoundboardMuted)
		.to_owned()
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager.alter_table(add_column()).await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager.alter_table(drop_column()).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn up_adds_a_non_null_boolean_defaulting_to_false() {
		assert_eq!(
			add_column().to_string(PostgresQueryBuilder),
			r#"ALTER TABLE "party_members" ADD COLUMN "soundboard_muted" bool NOT NULL DEFAULT FALSE"#
		);
	}

	#[test]
	fn down_drops_the_column() {
		assert_eq!(
			drop_column().to_string(PostgresQueryBuilder),
			r#"ALTER TABLE "party_members" DROP COLUMN "soundboard_muted""#
		);
	}
}
