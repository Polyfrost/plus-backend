use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "cosmetic_set_requirement")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub set_id: i32,
	pub cosmetic_id: Option<i32>,
	pub group_id: Option<i32>,
	#[sea_orm(column_type = "Text", nullable)]
	pub variant_key: Option<String>,
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
		belongs_to = "super::cosmetic::Entity",
		from = "Column::CosmeticId",
		to = "super::cosmetic::Column::Id",
		on_update = "NoAction",
		on_delete = "NoAction"
	)]
	Cosmetic,
	#[sea_orm(
		belongs_to = "super::cosmetic_group::Entity",
		from = "Column::GroupId",
		to = "super::cosmetic_group::Column::Id",
		on_update = "NoAction",
		on_delete = "NoAction"
	)]
	CosmeticGroup,
}

impl Related<super::cosmetic_set::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::CosmeticSet.def()
	}
}

impl Related<super::cosmetic::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Cosmetic.def()
	}
}

impl Related<super::cosmetic_group::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::CosmeticGroup.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
