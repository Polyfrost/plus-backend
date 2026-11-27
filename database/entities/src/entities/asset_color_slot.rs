use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "asset_color_slot")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub asset_id: i32,
	#[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
	pub slot: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::asset::Entity",
		from = "Column::AssetId",
		to = "super::asset::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Asset,
}

impl Related<super::asset::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Asset.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
