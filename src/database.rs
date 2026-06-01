use entities::{
	prelude::*,
	sea_orm_active_enums::{TransactionProvider, TransactionStatus},
	transaction, user,
};
use sea_orm::{ActiveValue, DbErr, EntityTrait, QueryFilter, prelude::*};
use uuid::Uuid;

pub(crate) trait DatabaseUserExt {
	/// Gets a [user::Model] given a specific Minecraft UUID, or else inserts a
	/// new user into the database.
	async fn get_or_create(
		db: &impl ConnectionTrait,
		minecraft_uuid: Uuid,
	) -> Result<user::Model, DbErr>;
}

pub(crate) trait DatabaseTransactionExt {
	async fn get_or_create_tebex(
		db: &impl ConnectionTrait,
		player_id: i32,
		transaction_id: &str,
		raw_metadata: serde_json::Value,
	) -> Result<transaction::Model, DbErr>;
}

impl DatabaseUserExt for User {
	async fn get_or_create(
		db: &impl ConnectionTrait,
		minecraft_uuid: Uuid,
	) -> Result<user::Model, DbErr> {
		let existing = User::find()
			.filter(user::Column::MinecraftUuid.eq(minecraft_uuid))
			.one(db)
			.await?;

		Ok(match existing {
			Some(model) => model,
			None => {
				User::insert(user::ActiveModel {
					minecraft_uuid: ActiveValue::Set(minecraft_uuid),
					..Default::default()
				})
				.exec_with_returning(db)
				.await?
			}
		})
	}
}

impl DatabaseTransactionExt for Transaction {
	async fn get_or_create_tebex(
		db: &impl ConnectionTrait,
		player_id: i32,
		transaction_id: &str,
		raw_metadata: serde_json::Value,
	) -> Result<transaction::Model, DbErr> {
		if let Some(existing) = Transaction::find()
			.filter(transaction::Column::Provider.eq(TransactionProvider::Tebex))
			.filter(transaction::Column::ProviderTransactionId.eq(transaction_id))
			.one(db)
			.await?
		{
			return Ok(existing);
		}

		Transaction::insert(transaction::ActiveModel {
			player_id: ActiveValue::Set(player_id),
			provider: ActiveValue::Set(TransactionProvider::Tebex),
			provider_transaction_id: ActiveValue::Set(Some(transaction_id.to_string())),
			status: ActiveValue::Set(TransactionStatus::Completed),
			raw_metadata: ActiveValue::Set(raw_metadata),
			..Default::default()
		})
		.exec_with_returning(db)
		.await
	}
}
