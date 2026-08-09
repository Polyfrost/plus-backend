use sea_orm_migration::{
	prelude::{extension::postgres::Type, *},
	sea_orm::{EnumIter, Iterable as _},
};

use crate::m20250917_163702_create_users_table::User;

#[derive(DeriveIden)]
pub struct RelationshipKind;

#[derive(DeriveIden, EnumIter)]
pub enum RelationshipKindVariants {
	Friend,
	BestFriend,
}

#[derive(DeriveIden)]
pub struct RelationshipRequestStatus;

#[derive(DeriveIden, EnumIter)]
pub enum RelationshipRequestStatusVariants {
	Pending,
	Accepted,
	Declined,
	Cancelled,
}

#[derive(DeriveIden)]
pub struct GroupKind;

#[derive(DeriveIden, EnumIter)]
pub enum GroupKindVariants {
	Dm,
	Group,
}

#[derive(DeriveIden)]
pub struct GroupMemberRole;

#[derive(DeriveIden, EnumIter)]
pub enum GroupMemberRoleVariants {
	Owner,
	Member,
}

#[derive(DeriveIden)]
pub struct SessionInviteStatus;

#[derive(DeriveIden, EnumIter)]
pub enum SessionInviteStatusVariants {
	Pending,
	Accepted,
	Declined,
	Expired,
}

#[derive(DeriveIden)]
pub enum Relationships {
	Table,
	Id,
	UserAId,
	UserBId,
	Kind,
	CreatedAt,
}

#[derive(DeriveIden)]
pub enum Blocks {
	Table,
	Id,
	BlockerId,
	BlockedId,
	CreatedAt,
}

#[derive(DeriveIden)]
pub enum RelationshipRequests {
	Table,
	Id,
	SenderId,
	RecipientId,
	Status,
	CreatedAt,
	RespondedAt,
}

#[derive(DeriveIden)]
pub enum Groups {
	Table,
	Id,
	Kind,
	Name,
	OwnerId,
	CreatedAt,
}

#[derive(DeriveIden)]
pub enum GroupMembers {
	Table,
	GroupId,
	UserId,
	Role,
	JoinedAt,
}

#[derive(DeriveIden)]
pub enum GroupMessages {
	Table,
	Id,
	GroupId,
	SenderId,
	Content,
	SentAt,
	EditedAt,
	DeletedAt,
	IdempotencyKey,
}

#[derive(DeriveIden)]
pub enum GroupMessageReads {
	Table,
	GroupId,
	UserId,
	LastReadMessageId,
	UpdatedAt,
}

#[derive(DeriveIden)]
pub enum GlobalChatMessages {
	Table,
	Id,
	SenderId,
	Content,
	SentAt,
}

#[derive(DeriveIden)]
pub enum GameSessions {
	Table,
	Id,
	HostId,
	EosSessionId,
	CreatedAt,
	ExpiresAt,
}

#[derive(DeriveIden)]
pub enum SessionInvites {
	Table,
	Id,
	SessionId,
	SenderId,
	RecipientId,
	Status,
	CreatedAt,
	RespondedAt,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.create_type(
				Type::create()
					.as_enum(RelationshipKind)
					.values(RelationshipKindVariants::iter())
					.to_owned(),
			)
			.await?;
		manager
			.create_type(
				Type::create()
					.as_enum(RelationshipRequestStatus)
					.values(RelationshipRequestStatusVariants::iter())
					.to_owned(),
			)
			.await?;
		manager
			.create_type(
				Type::create()
					.as_enum(GroupKind)
					.values(GroupKindVariants::iter())
					.to_owned(),
			)
			.await?;
		manager
			.create_type(
				Type::create()
					.as_enum(GroupMemberRole)
					.values(GroupMemberRoleVariants::iter())
					.to_owned(),
			)
			.await?;
		manager
			.create_type(
				Type::create()
					.as_enum(SessionInviteStatus)
					.values(SessionInviteStatusVariants::iter())
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(Relationships::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(Relationships::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(Relationships::UserAId).integer().not_null())
					.col(ColumnDef::new(Relationships::UserBId).integer().not_null())
					.col(
						ColumnDef::new(Relationships::Kind)
							.custom(RelationshipKind)
							.not_null(),
					)
					.col(
						ColumnDef::new(Relationships::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Relationships::Table, Relationships::UserAId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Relationships::Table, Relationships::UserBId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.index(
						Index::create()
							.unique()
							.col(Relationships::UserAId)
							.col(Relationships::UserBId),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(Blocks::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(Blocks::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(Blocks::BlockerId).integer().not_null())
					.col(ColumnDef::new(Blocks::BlockedId).integer().not_null())
					.col(
						ColumnDef::new(Blocks::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Blocks::Table, Blocks::BlockerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Blocks::Table, Blocks::BlockedId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.index(
						Index::create()
							.unique()
							.col(Blocks::BlockerId)
							.col(Blocks::BlockedId),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(RelationshipRequests::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(RelationshipRequests::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(RelationshipRequests::SenderId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(RelationshipRequests::RecipientId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(RelationshipRequests::Status)
							.custom(RelationshipRequestStatus)
							.not_null()
							.default("pending"),
					)
					.col(
						ColumnDef::new(RelationshipRequests::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(RelationshipRequests::RespondedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.foreign_key(
						ForeignKey::create()
							.from(RelationshipRequests::Table, RelationshipRequests::SenderId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								RelationshipRequests::Table,
								RelationshipRequests::RecipientId,
							)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		// Only one pending request may exist between any unordered pair of users
		// at a time, regardless of who sent it.
		manager
			.get_connection()
			.execute_unprepared(
				r#"
				CREATE UNIQUE INDEX relationship_requests_pending_pair_idx
					ON relationship_requests (LEAST(sender_id, recipient_id), GREATEST(sender_id, recipient_id))
					WHERE status = 'pending';
				"#,
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(Groups::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(Groups::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(Groups::Kind).custom(GroupKind).not_null())
					.col(ColumnDef::new(Groups::Name).string().null())
					.col(ColumnDef::new(Groups::OwnerId).integer().null())
					.col(
						ColumnDef::new(Groups::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(Groups::Table, Groups::OwnerId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(GroupMembers::Table)
					.if_not_exists()
					.col(ColumnDef::new(GroupMembers::GroupId).integer().not_null())
					.col(ColumnDef::new(GroupMembers::UserId).integer().not_null())
					.col(
						ColumnDef::new(GroupMembers::Role)
							.custom(GroupMemberRole)
							.not_null()
							.default("member"),
					)
					.col(
						ColumnDef::new(GroupMembers::JoinedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(GroupMembers::GroupId)
							.col(GroupMembers::UserId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(GroupMembers::Table, GroupMembers::GroupId)
							.to(Groups::Table, Groups::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(GroupMembers::Table, GroupMembers::UserId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(GroupMessages::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(GroupMessages::Id)
							.big_integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(GroupMessages::GroupId).integer().not_null())
					.col(
						ColumnDef::new(GroupMessages::SenderId)
							.integer()
							.not_null(),
					)
					.col(ColumnDef::new(GroupMessages::Content).text().not_null())
					.col(
						ColumnDef::new(GroupMessages::SentAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(GroupMessages::EditedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(
						ColumnDef::new(GroupMessages::DeletedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.col(ColumnDef::new(GroupMessages::IdempotencyKey).uuid().null())
					.foreign_key(
						ForeignKey::create()
							.from(GroupMessages::Table, GroupMessages::GroupId)
							.to(Groups::Table, Groups::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(GroupMessages::Table, GroupMessages::SenderId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.index(
						Index::create()
							.unique()
							.col(GroupMessages::GroupId)
							.col(GroupMessages::IdempotencyKey),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("group_messages_group_sent_at_idx")
					.table(GroupMessages::Table)
					.col(GroupMessages::GroupId)
					.col(GroupMessages::SentAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(GroupMessageReads::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(GroupMessageReads::GroupId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(GroupMessageReads::UserId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(GroupMessageReads::LastReadMessageId)
							.big_integer()
							.null(),
					)
					.col(
						ColumnDef::new(GroupMessageReads::UpdatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.primary_key(
						Index::create()
							.col(GroupMessageReads::GroupId)
							.col(GroupMessageReads::UserId),
					)
					.foreign_key(
						ForeignKey::create()
							.from(GroupMessageReads::Table, GroupMessageReads::GroupId)
							.to(Groups::Table, Groups::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(GroupMessageReads::Table, GroupMessageReads::UserId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(
								GroupMessageReads::Table,
								GroupMessageReads::LastReadMessageId,
							)
							.to(GroupMessages::Table, GroupMessages::Id)
							.on_delete(ForeignKeyAction::SetNull),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(GlobalChatMessages::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(GlobalChatMessages::Id)
							.big_integer()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(GlobalChatMessages::SenderId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(GlobalChatMessages::Content)
							.text()
							.not_null(),
					)
					.col(
						ColumnDef::new(GlobalChatMessages::SentAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.foreign_key(
						ForeignKey::create()
							.from(GlobalChatMessages::Table, GlobalChatMessages::SenderId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_index(
				Index::create()
					.name("global_chat_messages_sent_at_idx")
					.table(GlobalChatMessages::Table)
					.col(GlobalChatMessages::SentAt)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(GameSessions::Table)
					.if_not_exists()
					.col(ColumnDef::new(GameSessions::Id).uuid().primary_key())
					.col(ColumnDef::new(GameSessions::HostId).integer().not_null())
					.col(ColumnDef::new(GameSessions::EosSessionId).string().null())
					.col(
						ColumnDef::new(GameSessions::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(GameSessions::ExpiresAt)
							.timestamp_with_time_zone()
							.not_null(),
					)
					.foreign_key(
						ForeignKey::create()
							.from(GameSessions::Table, GameSessions::HostId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		manager
			.create_table(
				Table::create()
					.table(SessionInvites::Table)
					.if_not_exists()
					.col(
						ColumnDef::new(SessionInvites::Id)
							.integer()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(SessionInvites::SessionId).uuid().not_null())
					.col(
						ColumnDef::new(SessionInvites::SenderId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(SessionInvites::RecipientId)
							.integer()
							.not_null(),
					)
					.col(
						ColumnDef::new(SessionInvites::Status)
							.custom(SessionInviteStatus)
							.not_null()
							.default("pending"),
					)
					.col(
						ColumnDef::new(SessionInvites::CreatedAt)
							.timestamp_with_time_zone()
							.not_null()
							.default(Expr::current_timestamp()),
					)
					.col(
						ColumnDef::new(SessionInvites::RespondedAt)
							.timestamp_with_time_zone()
							.null(),
					)
					.foreign_key(
						ForeignKey::create()
							.from(SessionInvites::Table, SessionInvites::SessionId)
							.to(GameSessions::Table, GameSessions::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(SessionInvites::Table, SessionInvites::SenderId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.foreign_key(
						ForeignKey::create()
							.from(SessionInvites::Table, SessionInvites::RecipientId)
							.to(User::Table, User::Id)
							.on_delete(ForeignKeyAction::Cascade),
					)
					.to_owned(),
			)
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager
			.drop_table(Table::drop().table(SessionInvites::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(GameSessions::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(GlobalChatMessages::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(GroupMessageReads::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(GroupMessages::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(GroupMembers::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Groups::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(RelationshipRequests::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Blocks::Table).to_owned())
			.await?;
		manager
			.drop_table(Table::drop().table(Relationships::Table).to_owned())
			.await?;

		manager
			.drop_type(Type::drop().name(SessionInviteStatus).to_owned())
			.await?;
		manager
			.drop_type(Type::drop().name(GroupMemberRole).to_owned())
			.await?;
		manager
			.drop_type(Type::drop().name(GroupKind).to_owned())
			.await?;
		manager
			.drop_type(Type::drop().name(RelationshipRequestStatus).to_owned())
			.await?;
		manager
			.drop_type(Type::drop().name(RelationshipKind).to_owned())
			.await?;

		Ok(())
	}
}
