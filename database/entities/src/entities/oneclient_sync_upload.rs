use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "oneclient_sync_upload")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub id: Uuid,
	#[sea_orm(unique)]
	pub record_id: i32,
	pub revision: i64,
	#[sea_orm(column_type = "Text")]
	pub staging_key: String,
	pub format: i32,
	#[sea_orm(column_type = "Text")]
	pub name: String,
	#[sea_orm(column_type = "Text")]
	pub mc_version: String,
	#[sea_orm(column_type = "Text")]
	pub mc_loader: String,
	#[sea_orm(column_type = "Text")]
	pub kind: String,
	pub size: i64,
	#[sea_orm(column_type = "Text")]
	pub sha512: String,
	pub provisioned: bool,
	#[sea_orm(column_type = "Text")]
	pub device: String,
	pub uploader_id: Option<i32>,
	pub expires_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::oneclient_sync_record::Entity",
		from = "Column::RecordId",
		to = "super::oneclient_sync_record::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	OneclientSyncRecord,
}

impl Related<super::oneclient_sync_record::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::OneclientSyncRecord.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
