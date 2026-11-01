use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "oneclient_share_collaborator")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub record_id: i32,
	pub player_id: i32,
	pub added_at: DateTimeWithTimeZone,
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
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::PlayerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl ActiveModelBehavior for ActiveModel {}
