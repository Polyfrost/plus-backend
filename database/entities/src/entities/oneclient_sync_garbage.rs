use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "oneclient_sync_garbage")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i64,
	#[sea_orm(column_type = "Text")]
	pub object_key: String,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
