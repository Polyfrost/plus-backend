use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "player_cosmetic_colors")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub player_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub cosmetic_id: i32,
	#[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
	pub slot: String,
	pub color: Option<i32>,
	pub updated_at: DateTimeWithTimeZone,
	pub active: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::cosmetic::Entity",
		from = "Column::CosmeticId",
		to = "super::cosmetic::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Cosmetic,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::PlayerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::cosmetic::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Cosmetic.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
