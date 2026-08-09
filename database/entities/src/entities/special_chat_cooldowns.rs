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
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
