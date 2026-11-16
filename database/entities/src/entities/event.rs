//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "event")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	#[sea_orm(column_type = "Text")]
	pub name: String,
	pub starts_at: DateTimeWithTimeZone,
	pub ends_at: DateTimeWithTimeZone,
	#[sea_orm(column_type = "JsonBinary")]
	pub config: Json,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(has_many = "super::event_claim::Entity")]
	EventClaim,
}

impl Related<super::event_claim::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::EventClaim.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
