use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "player_skin")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub player_id: i32,
	#[sea_orm(column_type = "Text")]
	pub name: String,
	#[sea_orm(column_type = "Text")]
	pub model: String,
	#[sea_orm(column_type = "Text")]
	pub hash: String,
	pub favorite: bool,
	pub created_at: DateTimeWithTimeZone,
	pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(has_many = "super::player_outfit::Entity")]
	PlayerOutfit,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::PlayerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::player_outfit::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::PlayerOutfit.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
