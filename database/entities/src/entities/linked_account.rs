//! `SeaORM` Entity

use super::sea_orm_active_enums::LinkProvider;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "linked_account")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub user_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub provider: LinkProvider,
	#[sea_orm(column_type = "Text")]
	pub provider_user_id: String,
	#[sea_orm(column_type = "Text", nullable)]
	pub username: Option<String>,
	pub linked_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::UserId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
