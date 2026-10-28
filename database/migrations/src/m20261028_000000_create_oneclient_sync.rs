use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
}

#[derive(DeriveIden)]
enum OneclientSyncRecord {
	Table,
	Id,
	PlayerId,
	Code,
	Revision,
	Format,
	Name,
	McVersion,
	McLoader,
	Kind,
	Size,
	Sha512,
	Provisioned,
	UpdatedBy,
	ObjectKey,
	Deleted,
	CreatedAt,
	UpdatedAt,
}

#[derive(DeriveIden)]
enum OneclientSyncUpload {
	Table,
	Id,
	RecordId,
	Revision,
	StagingKey,
	Format,
	Name,
	McVersion,
	McLoader,
	Kind,
	Size,
	Sha512,
	Provisioned,
	Device,
	ExpiresAt,
}

#[derive(DeriveIden)]
enum OneclientSyncGarbage {
	Table,
	Id,
	ObjectKey,
	CreatedAt,
}

const RECORD_PLAYER_CODE_IDX: &str = "oneclient_sync_record_player_id_code_key";
const RECORD_DELETED_UPDATED_IDX: &str = "oneclient_sync_record_deleted_updated_at_idx";
const UPLOAD_RECORD_IDX: &str = "oneclient_sync_upload_record_id_key";
const UPLOAD_EXPIRES_IDX: &str = "oneclient_sync_upload_expires_at_idx";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_table(
				Table::create()
					.table(OneclientSyncRecord::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(OneclientSyncRecord::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::PlayerId)
							.integer()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncRecord::Code).text().not_null())
					.col(
						ColumnDef::new(OneclientSyncRecord::Revision)
							.big_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::Format)
							.integer()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncRecord::Name).text().not_null())
					.col(
						ColumnDef::new(OneclientSyncRecord::McVersion)
							.text()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::McLoader)
							.text()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncRecord::Kind).text().not_null())
					.col(
						ColumnDef::new(OneclientSyncRecord::Size)
							.big_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::Sha512)
							.text()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::Provisioned)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::UpdatedBy)
							.text()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncRecord::ObjectKey).text().null())
					.col(
						ColumnDef::new(OneclientSyncRecord::Deleted)
							.boolean()
							.not_null()
							.default(false),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(OneclientSyncRecord::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(OneclientSyncRecord::Table, OneclientSyncRecord::PlayerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(RECORD_PLAYER_CODE_IDX)
					.table(OneclientSyncRecord::Table)
					.col(OneclientSyncRecord::PlayerId)
					.col(OneclientSyncRecord::Code)
					.unique()
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(RECORD_DELETED_UPDATED_IDX)
					.table(OneclientSyncRecord::Table)
					.col(OneclientSyncRecord::Deleted)
					.col(OneclientSyncRecord::UpdatedAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(OneclientSyncUpload::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(OneclientSyncUpload::Id)
							.uuid()
							.not_null()
							.primary_key(),
					)
					.col(
						ColumnDef::new(OneclientSyncUpload::RecordId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncUpload::Revision)
							.big_integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncUpload::StagingKey)
							.text()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncUpload::Format)
							.integer()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncUpload::Name).text().not_null())
					.col(
						ColumnDef::new(OneclientSyncUpload::McVersion)
							.text()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncUpload::McLoader)
							.text()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncUpload::Kind).text().not_null())
					.col(
						ColumnDef::new(OneclientSyncUpload::Size)
							.big_integer()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncUpload::Sha512).text().not_null())
					.col(
						ColumnDef::new(OneclientSyncUpload::Provisioned)
							.boolean()
							.not_null(),
					)
					.col(ColumnDef::new(OneclientSyncUpload::Device).text().not_null())
					.col(
						ColumnDef::new(OneclientSyncUpload::ExpiresAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.foreign_key(
						ForeignKey::create()
							.from(OneclientSyncUpload::Table, OneclientSyncUpload::RecordId)
							.to(OneclientSyncRecord::Table, OneclientSyncRecord::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(UPLOAD_RECORD_IDX)
					.table(OneclientSyncUpload::Table)
					.col(OneclientSyncUpload::RecordId)
					.unique()
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(UPLOAD_EXPIRES_IDX)
					.table(OneclientSyncUpload::Table)
					.col(OneclientSyncUpload::ExpiresAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(OneclientSyncGarbage::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(OneclientSyncGarbage::Id)
							.big_integer()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(OneclientSyncGarbage::ObjectKey)
							.text()
							.not_null(),
					)
					.col(
						ColumnDef::new(OneclientSyncGarbage::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.to_owned(),
			)
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(OneclientSyncGarbage::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(OneclientSyncUpload::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(OneclientSyncRecord::Table).to_owned())
			.await?;
		Ok(())
	}
}
