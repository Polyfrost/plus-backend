//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "game_sessions")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub id: Uuid,
	pub host_id: i32,
	pub eos_session_id: Option<String>,
	pub created_at: DateTimeWithTimeZone,
	pub expires_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::HostId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Host,
	#[sea_orm(has_many = "super::session_invites::Entity")]
	SessionInvites,
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Host.def()
	}
}

impl Related<super::session_invites::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::SessionInvites.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
