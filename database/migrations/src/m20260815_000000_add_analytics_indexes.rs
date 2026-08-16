use sea_orm_migration::prelude::*;

use crate::{
	m20250917_163702_create_users_table::User,
	m20260629_000000_create_daily_playtime::DailyPlaytime,
	m20260720_000000_create_tracked_links::TrackedLinkHits,
	m20260731_000000_create_social_tables::{
		Blocks, GameSessions, GroupMessages, RelationshipRequests, Relationships,
		SessionInvites,
	},
};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum UserColumns {
	CreatedAt,
}

#[derive(DeriveIden)]
enum PlayerOwnedCosmetic {
	Table,
	AcquiredAt,
	CosmeticId,
	TransactionId,
}

#[derive(DeriveIden)]
enum PlayerEquippedCosmetic {
	Table,
	CosmeticId,
}

#[derive(DeriveIden)]
enum Transaction {
	Table,
	CreatedAt,
	PlayerId,
	Status,
	Buyer,
}

const INDEX_NAMES: &[&str] = &[
	"daily_playtime_day_player_idx",
	"user_created_at_idx",
	"user_created_at_utc_date_idx",
	"player_owned_cosmetic_acquired_at_idx",
	"player_owned_cosmetic_cosmetic_id_idx",
	"player_owned_cosmetic_transaction_id_idx",
	"player_equipped_cosmetic_cosmetic_id_idx",
	"transaction_created_at_idx",
	"transaction_player_created_at_idx",
	"transaction_status_created_at_idx",
	"transaction_buyer_created_at_idx",
	"relationship_requests_created_at_idx",
	"relationship_requests_status_responded_at_idx",
	"relationships_created_at_idx",
	"blocks_created_at_idx",
	"session_invites_created_at_idx",
	"game_sessions_created_at_idx",
	"tracked_link_hits_created_at_idx",
	"group_messages_sent_at_idx",
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// The composite primary keys all lead with `player_id`, so none of them
		// serve a cross-player time scan.
		let mut indexes = vec![
			Index::create()
				.name(INDEX_NAMES[0])
				.table(DailyPlaytime::Table)
				.col(DailyPlaytime::Day)
				.col(DailyPlaytime::PlayerId)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[1])
				.table(User::Table)
				.col(UserColumns::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[2])
				.table(User::Table)
				.col(Expr::cust("(created_at AT TIME ZONE 'UTC')::date"))
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[3])
				.table(PlayerOwnedCosmetic::Table)
				.col(PlayerOwnedCosmetic::AcquiredAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[4])
				.table(PlayerOwnedCosmetic::Table)
				.col(PlayerOwnedCosmetic::CosmeticId)
				.to_owned(),
			// Also used by the stripe refund path, which deletes owned
			// cosmetics by transaction_id.
			Index::create()
				.name(INDEX_NAMES[5])
				.table(PlayerOwnedCosmetic::Table)
				.col(PlayerOwnedCosmetic::TransactionId)
				.and_where(Expr::col(PlayerOwnedCosmetic::TransactionId).is_not_null())
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[6])
				.table(PlayerEquippedCosmetic::Table)
				.col(PlayerEquippedCosmetic::CosmeticId)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[7])
				.table(Transaction::Table)
				.col(Transaction::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[8])
				.table(Transaction::Table)
				.col(Transaction::PlayerId)
				.col(Transaction::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[9])
				.table(Transaction::Table)
				.col(Transaction::Status)
				.col(Transaction::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[10])
				.table(Transaction::Table)
				.col(Transaction::Buyer)
				.col(Transaction::CreatedAt)
				.and_where(Expr::col(Transaction::Buyer).is_not_null())
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[11])
				.table(RelationshipRequests::Table)
				.col(RelationshipRequests::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[12])
				.table(RelationshipRequests::Table)
				.col(RelationshipRequests::Status)
				.col(RelationshipRequests::RespondedAt)
				.and_where(Expr::col(RelationshipRequests::RespondedAt).is_not_null())
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[13])
				.table(Relationships::Table)
				.col(Relationships::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[14])
				.table(Blocks::Table)
				.col(Blocks::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[15])
				.table(SessionInvites::Table)
				.col(SessionInvites::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[16])
				.table(GameSessions::Table)
				.col(GameSessions::CreatedAt)
				.to_owned(),
			Index::create()
				.name(INDEX_NAMES[17])
				.table(TrackedLinkHits::Table)
				.col(TrackedLinkHits::CreatedAt)
				.to_owned(),
			// group_messages already has (group_id, sent_at), which cannot
			// serve a filter that spans groups.
			Index::create()
				.name(INDEX_NAMES[18])
				.table(GroupMessages::Table)
				.col(GroupMessages::SentAt)
				.to_owned(),
		];

		for index in &mut indexes {
			manager
				.create_index(index.if_not_exists().to_owned())
				.await?;
		}

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		for name in INDEX_NAMES.iter().rev() {
			manager
				.drop_index(Index::drop().if_exists().name(*name).to_owned())
				.await?;
		}

		Ok(())
	}
}
