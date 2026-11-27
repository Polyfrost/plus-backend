use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "player_set_discovery")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub player_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub set_id: i32,
	pub cosmetic_ids: Vec<i32>,
	pub discovered_at: DateTimeWithTimeZone,
	pub delivered_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::cosmetic_set::Entity",
		from = "Column::SetId",
		to = "super::cosmetic_set::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	CosmeticSet,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::PlayerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::cosmetic_set::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::CosmeticSet.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
