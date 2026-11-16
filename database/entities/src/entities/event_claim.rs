//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "event_claim")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub event_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub user_id: i32,
	#[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
	pub slot: String,
	#[sea_orm(column_type = "JsonBinary")]
	pub granted: Json,
	pub claimed_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::event::Entity",
		from = "Column::EventId",
		to = "super::event::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Event,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::UserId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::event::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Event.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
