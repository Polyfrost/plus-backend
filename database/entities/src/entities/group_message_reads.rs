//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "group_message_reads")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub group_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub user_id: i32,
	pub last_read_message_id: Option<i64>,
	pub updated_at: DateTimeWithTimeZone,
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
		from = "Column::UserId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
	#[sea_orm(
		belongs_to = "super::group_messages::Entity",
		from = "Column::LastReadMessageId",
		to = "super::group_messages::Column::Id",
		on_update = "NoAction",
		on_delete = "SetNull"
	)]
	GroupMessages,
}

impl ActiveModelBehavior for ActiveModel {}
