//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "role")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	#[sea_orm(column_type = "Text", unique)]
	pub name: String,
	pub color: Option<i32>,
	pub position: i32,
	pub permissions: i64,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(has_many = "super::user_role::Entity")]
	UserRole,
}

impl Related<super::user_role::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::UserRole.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
