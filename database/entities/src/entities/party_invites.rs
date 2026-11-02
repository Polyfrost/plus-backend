use super::sea_orm_active_enums::PartyInviteStatus;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "party_invites")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub id: Uuid,
	pub party_id: Uuid,
	pub sender_id: i32,
	pub recipient_id: i32,
	pub status: PartyInviteStatus,
	pub created_at: DateTimeWithTimeZone,
	pub expires_at: DateTimeWithTimeZone,
	pub responded_at: Option<DateTimeWithTimeZone>,
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
		from = "Column::SenderId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Sender,
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::RecipientId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "Cascade"
	)]
	Recipient,
}

impl Related<super::parties::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::Parties.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
