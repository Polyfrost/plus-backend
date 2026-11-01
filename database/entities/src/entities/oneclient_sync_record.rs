use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "oneclient_sync_record")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub player_id: i32,
	#[sea_orm(column_type = "Text", unique)]
	pub code: String,
	pub revision: i64,
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
	pub updated_by: String,
	#[sea_orm(column_type = "Text", nullable)]
	pub object_key: Option<String>,
	#[sea_orm(column_type = "Text")]
	pub visibility: String,
	pub expires_at: Option<DateTimeWithTimeZone>,
	#[sea_orm(column_type = "JsonBinary", nullable)]
	pub contents: Option<Json>,
	pub deleted: bool,
	pub created_at: DateTimeWithTimeZone,
	pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(has_one = "super::oneclient_sync_upload::Entity")]
	OneclientSyncUpload,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::PlayerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::oneclient_sync_upload::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::OneclientSyncUpload.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
