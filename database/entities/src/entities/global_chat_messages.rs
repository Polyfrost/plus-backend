//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "global_chat_messages")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i64,
	pub sender_id: i32,
	pub content: String,
	pub sent_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::SenderId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Sender,
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Sender.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
