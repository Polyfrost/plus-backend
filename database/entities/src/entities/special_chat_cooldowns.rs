//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "special_chat_cooldowns")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub sender_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub target_id: i32,
	pub last_sent_at: DateTimeWithTimeZone,
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
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::TargetId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Target,
}

impl ActiveModelBehavior for ActiveModel {}
