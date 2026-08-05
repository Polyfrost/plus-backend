//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "group_messages")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i64,
	pub group_id: i32,
	pub sender_id: i32,
	pub content: String,
	pub sent_at: DateTimeWithTimeZone,
	pub edited_at: Option<DateTimeWithTimeZone>,
	pub deleted_at: Option<DateTimeWithTimeZone>,
	pub idempotency_key: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::groups::Entity",
		from = "Column::GroupId",
		to = "super::groups::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Groups,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::SenderId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Sender,
}

impl Related<super::groups::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Groups.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
