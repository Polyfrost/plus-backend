//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "dm_delegates")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub owner_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub delegate_id: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
