//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "oidc_signing_keys")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub kid: String,
	pub private_key_der: Vec<u8>,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
