//! `SeaORM` Entity

use super::sea_orm_active_enums::GroupMemberRole;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "group_members")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub group_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub user_id: i32,
	pub role: GroupMemberRole,
	pub joined_at: DateTimeWithTimeZone,
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
}

impl Related<super::groups::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Groups.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
