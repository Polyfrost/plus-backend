use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "perf_profile")]
pub struct Model {
	#[sea_orm(primary_key, auto_increment = false)]
	pub id: Uuid,
	pub player_id: Option<i32>,
	pub created_at: DateTimeWithTimeZone,
	#[sea_orm(column_type = "Text")]
	pub pack_hash: String,
	#[sea_orm(column_type = "Text")]
	pub client_version: String,
	#[sea_orm(column_type = "Text")]
	pub minecraft_version: String,
	#[sea_orm(column_type = "Text")]
	pub loader: String,
	pub seconds: i32,
	pub size_bytes: i64,
	#[sea_orm(column_type = "Text")]
	pub s3_key: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
	#[sea_orm(
		belongs_to = "super::user::Entity",
		from = "Column::PlayerId",
		to = "super::user::Column::Id",
		on_update = "NoAction",
		on_delete = "SetNull"
	)]
	User,
}

impl Related<super::user::Entity> for Entity {
	fn to() -> RelationDef {
		Relation::User.def()
	}
}

impl ActiveModelBehavior for ActiveModel {}
