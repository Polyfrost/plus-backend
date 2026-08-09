//! `SeaORM` Entity

use super::sea_orm_active_enums::RelationshipRequestStatus;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "relationship_requests")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i32,
	pub sender_id: i32,
	pub recipient_id: i32,
	pub status: RelationshipRequestStatus,
	pub created_at: DateTimeWithTimeZone,
	pub responded_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
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

impl ActiveModelBehavior for ActiveModel {}
