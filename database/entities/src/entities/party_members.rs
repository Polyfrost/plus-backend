use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "party_members")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub party_id: Uuid,
	#[sea_orm(primary_key, auto_increment = false, unique)]
	pub user_id: i32,
	pub joined_at: DateTimeWithTimeZone,
	pub voice_hard_muted: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::parties::Entity",
		from = "Column::PartyId",
		to = "super::parties::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Parties,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::UserId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	User,
}

impl Related<super::parties::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Parties.def()
	}
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
