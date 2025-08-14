use entities::{prelude::*, user};
use sea_orm::{DbErr, EntityTrait, IntoActiveModel, QueryFilter, prelude::*};
use uuid::Uuid;

pub(crate) trait DatabaseUserExt {
	/// Gets a [user::Model] given a specific Minecraft UUID, or else inserts a
	/// new user into the database.
	async fn get_or_create(
		db: &impl ConnectionTrait,
		minecraft_uuid: Uuid
	) -> Result<user::Model, DbErr>;
}

impl DatabaseUserExt for User {
	async fn get_or_create(
		db: &impl ConnectionTrait,
		minecraft_uuid: Uuid
	) -> Result<user::Model, DbErr> {
		let existing = User::find()
			.filter(user::Column::MinecraftUuid.eq(minecraft_uuid))
			.one(db)
			.await?;

		Ok(match existing {
			Some(model) => model,
			None =>
				User::insert(
					user::Model {
						id: Uuid::now_v7(),
						minecraft_uuid
					}
					.into_active_model()
				)
				.exec_with_returning(db)
				.await?,
		})
	}
}
