use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

use crate::m20260731_000000_create_social_tables::Groups;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PENDING_INVITE_INDEX: &str = "party_invites_pending_party_recipient_idx";
const INVITE_RECIPIENT_INDEX: &str = "party_invites_recipient_status_idx";
const INVITE_SENDER_INDEX: &str = "party_invites_sender_status_idx";
const MEMBER_USER_INDEX: &str = "party_members_user_id_key";

#[derive(DeriveIden)]
struct PartyVoiceMode;

#[derive(DeriveIden, EnumIter)]
enum PartyVoiceModeVariants {
	Party,
	Proximity,
}

#[derive(DeriveIden)]
struct PartyInvitePolicy;

#[derive(DeriveIden, EnumIter)]
enum PartyInvitePolicyVariants {
	Leader,
	Members,
}

#[derive(DeriveIden)]
struct PartyInviteStatus;

#[derive(DeriveIden, EnumIter)]
enum PartyInviteStatusVariants {
	Pending,
	Accepted,
	Declined,
	Cancelled,
	Expired,
}

#[derive(DeriveIden)]
enum Parties {
	Table,
	Id,
	LeaderId,
	VoiceMode,
	InvitePolicy,
	ChatGroupId,
	CreatedAt,
	UpdatedAt,
}

#[derive(DeriveIden)]
enum PartyMembers {
	Table,
	PartyId,
	UserId,
	JoinedAt,
	VoiceHardMuted,
}

#[derive(DeriveIden)]
enum PartyInvites {
	Table,
	Id,
	PartyId,
	SenderId,
	RecipientId,
	Status,
	CreatedAt,
	ExpiresAt,
	RespondedAt,
}

#[derive(DeriveIden)]
enum User {
	Table,
	Id,
	EosProductUserIdVerifiedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(PartyVoiceMode)
					.values(PartyVoiceModeVariants::iter())
					.to_owned(),
			)
			.await?;
		manager
			.create_type(
				Type::create()
					.as_enum(PartyInvitePolicy)
					.values(PartyInvitePolicyVariants::iter())
					.to_owned(),
			)
			.await?;
		manager
			.create_type(
				Type::create()
					.as_enum(PartyInviteStatus)
					.values(PartyInviteStatusVariants::iter())
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(Parties::Table)
					.if_not_exists()
					.col(ColumnDef::new(Parties::Id).uuid().not_null().primary_key())
					.col(ColumnDef::new(Parties::LeaderId).integer().not_null())
					.col(
						ColumnDef::new(Parties::VoiceMode)
							.custom(PartyVoiceMode)
							.not_null()
							.default("party"),
					)
					.col(
						ColumnDef::new(Parties::InvitePolicy)
							.custom(PartyInvitePolicy)
							.not_null()
							.default("leader"),
					)
					.col(ColumnDef::new(Parties::ChatGroupId).integer().null())
					.col(
						ColumnDef::new(Parties::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(Parties::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Parties::Table, Parties::LeaderId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Parties::Table, Parties::ChatGroupId)
							.to(Groups::Table, Groups::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PartyMembers::Table)
					.if_not_exists()
					.col(ColumnDef::new(PartyMembers::PartyId).uuid().not_null())
					.col(ColumnDef::new(PartyMembers::UserId).integer().not_null())
					.col(
						ColumnDef::new(PartyMembers::JoinedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PartyMembers::VoiceHardMuted)
							.boolean()
							.not_null()
							.default(false),
					)
					.primary_key(
						Index::create()
							.col(PartyMembers::PartyId)
							.col(PartyMembers::UserId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PartyMembers::Table, PartyMembers::PartyId)
							.to(Parties::Table, Parties::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PartyMembers::Table, PartyMembers::UserId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name(MEMBER_USER_INDEX)
					.table(PartyMembers::Table)
					.col(PartyMembers::UserId)
					.unique()
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(PartyInvites::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(PartyInvites::Id)
							.uuid()
							.not_null()
							.primary_key(),
					)
					.col(ColumnDef::new(PartyInvites::PartyId).uuid().not_null())
					.col(ColumnDef::new(PartyInvites::SenderId).integer().not_null())
					.col(
						ColumnDef::new(PartyInvites::RecipientId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(PartyInvites::Status)
							.custom(PartyInviteStatus)
							.not_null()
							.default("pending"),
					)
					.col(
						ColumnDef::new(PartyInvites::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(PartyInvites::ExpiresAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.col(
						ColumnDef::new(PartyInvites::RespondedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PartyInvites::Table, PartyInvites::PartyId)
							.to(Parties::Table, Parties::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PartyInvites::Table, PartyInvites::SenderId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(PartyInvites::Table, PartyInvites::RecipientId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(PENDING_INVITE_INDEX)
					.table(PartyInvites::Table)
					.col(PartyInvites::PartyId)
					.col(PartyInvites::RecipientId)
					.unique()
					.and_where(Expr::col(PartyInvites::Status).eq("pending"))
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(INVITE_RECIPIENT_INDEX)
					.table(PartyInvites::Table)
					.col(PartyInvites::RecipientId)
					.col(PartyInvites::Status)
					.to_owned(),
			)
			.await?;
		manager
			.create_index(
				Index::create()
					.if_not_exists()
					.name(INVITE_SENDER_INDEX)
					.table(PartyInvites::Table)
					.col(PartyInvites::SenderId)
					.col(PartyInvites::Status)
					.to_owned(),
			)
			.await?;

		manager
			.alter_table(
				Table::alter()
					.table(User::Table)
					.add_column(
						ColumnDef::new(User::EosProductUserIdVerifiedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.to_owned(),
			)
			.await
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.alter_table(
				Table::alter()
					.table(User::Table)
					.drop_column(User::EosProductUserIdVerifiedAt)
					.to_owned(),
			)
			.await?;
		manager
			.drop_table(Table::drop().table(PartyInvites::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(PartyMembers::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Parties::Table).to_owned())
			.await?;
		for name in [
			PartyInviteStatus.to_string(),
			PartyInvitePolicy.to_string(),
			PartyVoiceMode.to_string(),
		] {
			manager
				.drop_type(Type::drop().name(Alias::new(name)).to_owned())
				.await?;
		}
		Ok(())
	}
}
