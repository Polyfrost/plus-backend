//! `SeaORM` Entity

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "rate_limit_lockout")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: i64,
	#[sea_orm(column_type = "Text")]
	pub limiter: String,
	#[sea_orm(column_type = "Text")]
	pub key: String,
	pub timeout_seconds: i64,
	pub request_limit: i64,
	pub window_seconds: i64,
	pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
