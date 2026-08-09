//! `SeaORM` Entity

use super::sea_orm_active_enums::RelationshipKind;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "relationships")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub user_a_id: i32,
	pub user_b_id: i32,
	pub kind: RelationshipKind,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::UserAId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	UserA,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::UserBId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	UserB,
}

impl ActiveModelBehavior for ActiveModel {}
