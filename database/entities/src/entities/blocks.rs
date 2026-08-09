//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "blocks")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub blocker_id: i32,
	pub blocked_id: i32,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::BlockerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Blocker,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::BlockedId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Blocked,
}

impl ActiveModelBehavior for ActiveModel {}
