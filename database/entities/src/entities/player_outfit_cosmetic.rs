use super::sea_orm_active_enums::BodySlot;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "player_outfit_cosmetic")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub outfit_id: i32,
	#[sea_orm(primary_key, auto_increment = false)]
	pub slot: BodySlot,
	pub cosmetic_id: i32,
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
		belongs_to = "super::player_outfit::Entity",
		from = "Column::OutfitId",
		to = "super::player_outfit::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	PlayerOutfit,
}

impl Related<super::cosmetic::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Cosmetic.def()
	}
}

impl Related<super::player_outfit::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::PlayerOutfit.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
