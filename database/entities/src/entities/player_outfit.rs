use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "player_outfit")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub player_id: i32,
	#[sea_orm(column_type = "Text")]
	pub name: String,
	pub favorite: bool,
	pub skin_id: Option<i32>,
	#[sea_orm(column_type = "Text", nullable)]
	pub mojang_cape_id: Option<String>,
	pub particle_color: Option<i32>,
	pub created_at: DateTimeWithTimeZone,
	pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(has_many = "super::player_outfit_cosmetic::Entity")]
	PlayerOutfitCosmetic,
	#[sea_orm(
		belongs_to = "super::player_skin::Entity",
		from = "Column::SkinId",
		to = "super::player_skin::Column::Id",
		on_update = "NoAction",
		on_delete = "SetNull"
	)]
	PlayerSkin,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::PlayerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::player_outfit_cosmetic::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::PlayerOutfitCosmetic.def()
	}
}

impl Related<super::player_skin::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::PlayerSkin.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl Related<super::cosmetic::Entity> for Entity {
	fn to() -> RelationDef {
		super::player_outfit_cosmetic::Relation::Cosmetic.def()
	}
	fn via() -> Option<RelationDef> {
		Some(super::player_outfit_cosmetic::Relation::PlayerOutfit.def().rev())
	}
}

impl ActiveModelBehavior for ActiveModel {}
