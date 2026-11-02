use super::sea_orm_active_enums::{PartyInvitePolicy, PartyVoiceMode};
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "parties")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub id: Uuid,
	pub leader_id: i32,
	pub voice_mode: PartyVoiceMode,
	pub invite_policy: PartyInvitePolicy,
	pub chat_group_id: Option<i32>,
	pub created_at: DateTimeWithTimeZone,
	pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::LeaderId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Leader,
	#[sea_orm(
		belongs_to = "super::groups::Entity",
		from = "Column::ChatGroupId",
		to = "super::groups::Column::Id",
		on_update = "NoAction",
		on_delete = "SetNull"
	)]
	Groups,
	#[sea_orm(has_many = "super::party_members::Entity")]
	PartyMembers,
	#[sea_orm(has_many = "super::party_invites::Entity")]
	PartyInvites,
}

impl Related<super::groups::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Groups.def()
	}
}

impl Related<super::party_members::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::PartyMembers.def()
	}
}

impl Related<super::party_invites::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::PartyInvites.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
