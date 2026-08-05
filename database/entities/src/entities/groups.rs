//! `SeaORM` Entity

use super::sea_orm_active_enums::GroupKind;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "groups")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub kind: GroupKind,
	pub name: Option<String>,
	pub owner_id: Option<i32>,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::OwnerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "SetNull"
	)]
	Owner,
	#[sea_orm(has_many = "super::group_members::Entity")]
	GroupMembers,
	#[sea_orm(has_many = "super::group_messages::Entity")]
	GroupMessages,
}

impl Related<super::group_members::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::GroupMembers.def()
	}
}

impl Related<super::group_messages::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::GroupMessages.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
