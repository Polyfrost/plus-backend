use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "cosmetic_set")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	#[sea_orm(column_type = "Text")]
	pub name: String,
	pub priority: i32,
	pub effect_asset_id: Option<i32>,
	pub active_from: Option<DateTimeWithTimeZone>,
	pub active_until: Option<DateTimeWithTimeZone>,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::asset::Entity",
		from = "Column::EffectAssetId",
		to = "super::asset::Column::Id",
		on_update = "NoAction",
		on_delete = "SetNull"
	)]
	Asset,
	#[sea_orm(has_many = "super::cosmetic_set_requirement::Entity")]
	CosmeticSetRequirement,
	#[sea_orm(has_many = "super::player_set_discovery::Entity")]
	PlayerSetDiscovery,
}

impl Related<super::asset::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Asset.def()
	}
}

impl Related<super::cosmetic_set_requirement::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::CosmeticSetRequirement.def()
	}
}

impl Related<super::player_set_discovery::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::PlayerSetDiscovery.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
