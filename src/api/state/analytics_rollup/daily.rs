use chrono::{Days, NaiveDate, Utc};
use entities::{
	analytics_daily, blocks, daily_playtime, game_sessions, group_messages,
	player_owned_cosmetic,
	prelude::*,
	relationship_requests, relationships,
	sea_orm_active_enums::{
		RelationshipRequestStatus, SessionInviteStatus, TransactionProvider,
		TransactionStatus,
	},
	session_invites, tracked_link_hits, transaction, user,
};
use sea_orm::{
	ActiveValue, ColumnTrait as _, DatabaseTransaction, DbErr, EntityTrait,
	FromQueryResult, PaginatorTrait as _, QueryFilter as _, QuerySelect as _, Select,
	prelude::DateTimeWithTimeZone,
	sea_query::{Alias, Expr, Func},
};

use super::{CountValue, count_in_day, day_bounds};

const WEEKLY_WINDOW: u64 = 6;
const MONTHLY_WINDOW: u64 = 29;

/// A refund flips the row's status, so matching `Completed` alone would take
/// the sale out of gross as well as into refunds, subtracting it twice.
const SALE_STATUSES: [TransactionStatus; 2] =
	[TransactionStatus::Completed, TransactionStatus::Refunded];

#[derive(Debug, Default, FromQueryResult)]
struct PlaytimeTotals {
	seconds: Option<i64>,
	sessions: Option<i64>,
}

/// Overwritten on conflict, so a re-run replaces rather than accumulates.
pub(super) fn rolled_up_columns() -> [analytics_daily::Column; 29] {
	use analytics_daily::Column as C;

	[
		C::ComputedAt,
		C::Dau,
		C::Wau,
		C::Mau,
		C::NewUsers,
		C::ReturningUsers,
		C::TotalUsers,
		C::PlaytimeSeconds,
		C::Sessions,
		C::CosmeticsAcquired,
		C::CosmeticsAcquiredPaid,
		C::CosmeticsAcquiredFree,
		C::CosmeticsAcquiredGranted,
		C::TransactionsCompleted,
		C::TransactionsRefunded,
		C::GiftTransactions,
		C::FriendRequestsSent,
		C::FriendRequestsAccepted,
		C::FriendshipsCreated,
		C::BlocksCreated,
		C::MessagesSent,
		C::GameSessionsCreated,
		C::SessionInvitesSent,
		C::SessionInvitesAccepted,
		C::TrackedLinkHits,
		C::GrossRevenueMinor,
		C::RefundAmountMinor,
		C::DiscountAmountMinor,
		C::PayingUsers,
	]
}

pub(super) async fn collect_day(
	txn: &DatabaseTransaction,
	day: NaiveDate,
) -> Result<analytics_daily::ActiveModel, DbErr> {
	let bounds = day_bounds(day);
	let (day_start, day_end) = bounds;
	let playtime = playtime_totals(txn, day).await?;
	let (paid_cosmetics, free_cosmetics) = stripe_acquisitions(txn, bounds).await?;

	Ok(analytics_daily::ActiveModel {
		day: ActiveValue::Set(day),
		computed_at: ActiveValue::Set(Utc::now().into()),

		dau: ActiveValue::Set(active_players(txn, day, day).await?),
		wau: ActiveValue::Set(
			active_players(txn, window_start(day, WEEKLY_WINDOW), day).await?,
		),
		mau: ActiveValue::Set(
			active_players(txn, window_start(day, MONTHLY_WINDOW), day).await?,
		),
		new_users: ActiveValue::Set(
			count_in_day(txn, User::find(), user::Column::CreatedAt, bounds).await?,
		),
		returning_users: ActiveValue::Set(returning_players(txn, day, day_start).await?),
		total_users: ActiveValue::Set(
			User::find()
				.filter(user::Column::CreatedAt.lt(day_end))
				.count(txn)
				.await?
				.try_into()
				.unwrap_or(i32::MAX),
		),
		playtime_seconds: ActiveValue::Set(playtime.seconds.unwrap_or_default()),
		sessions: ActiveValue::Set(
			playtime
				.sessions
				.unwrap_or_default()
				.try_into()
				.unwrap_or(i32::MAX),
		),

		cosmetics_acquired: ActiveValue::Set(
			count_in_day(
				txn,
				PlayerOwnedCosmetic::find(),
				player_owned_cosmetic::Column::AcquiredAt,
				bounds,
			)
			.await?,
		),
		cosmetics_acquired_paid: ActiveValue::Set(paid_cosmetics),
		cosmetics_acquired_free: ActiveValue::Set(free_cosmetics),
		cosmetics_acquired_granted: ActiveValue::Set(
			count_in_day(
				txn,
				PlayerOwnedCosmetic::find().filter(
					player_owned_cosmetic::Column::AcquiredVia
						.eq(TransactionProvider::AdminGrant),
				),
				player_owned_cosmetic::Column::AcquiredAt,
				bounds,
			)
			.await?,
		),
		transactions_completed: ActiveValue::Set(
			sales_on_day(bounds)
				.count(txn)
				.await?
				.try_into()
				.unwrap_or(i32::MAX),
		),
		transactions_refunded: ActiveValue::Set(
			count_in_day(
				txn,
				Transaction::find()
					.filter(transaction::Column::Status.eq(TransactionStatus::Refunded)),
				transaction::Column::CreatedAt,
				bounds,
			)
			.await?,
		),
		gift_transactions: ActiveValue::Set(
			count_in_day(
				txn,
				Transaction::find().filter(transaction::Column::Buyer.is_not_null()),
				transaction::Column::CreatedAt,
				bounds,
			)
			.await?,
		),

		friend_requests_sent: ActiveValue::Set(
			count_in_day(
				txn,
				RelationshipRequests::find(),
				relationship_requests::Column::CreatedAt,
				bounds,
			)
			.await?,
		),
		// Booked on the day it was answered, not the day it was sent.
		friend_requests_accepted: ActiveValue::Set(
			count_in_day(
				txn,
				RelationshipRequests::find().filter(
					relationship_requests::Column::Status
						.eq(RelationshipRequestStatus::Accepted),
				),
				relationship_requests::Column::RespondedAt,
				bounds,
			)
			.await?,
		),
		friendships_created: ActiveValue::Set(
			count_in_day(
				txn,
				Relationships::find(),
				relationships::Column::CreatedAt,
				bounds,
			)
			.await?,
		),
		blocks_created: ActiveValue::Set(
			count_in_day(txn, Blocks::find(), blocks::Column::CreatedAt, bounds).await?,
		),
		messages_sent: ActiveValue::Set(
			count_in_day(
				txn,
				GroupMessages::find().filter(group_messages::Column::DeletedAt.is_null()),
				group_messages::Column::SentAt,
				bounds,
			)
			.await?,
		),
		game_sessions_created: ActiveValue::Set(
			count_in_day(
				txn,
				GameSessions::find(),
				game_sessions::Column::CreatedAt,
				bounds,
			)
			.await?,
		),
		session_invites_sent: ActiveValue::Set(
			count_in_day(
				txn,
				SessionInvites::find(),
				session_invites::Column::CreatedAt,
				bounds,
			)
			.await?,
		),
		session_invites_accepted: ActiveValue::Set(
			count_in_day(
				txn,
				SessionInvites::find().filter(
					session_invites::Column::Status.eq(SessionInviteStatus::Accepted),
				),
				session_invites::Column::RespondedAt,
				bounds,
			)
			.await?,
		),
		gross_revenue_minor: ActiveValue::Set(
			minor_sum(txn, sales_on_day(bounds), transaction::Column::AmountMinor)
				.await?,
		),
		refund_amount_minor: ActiveValue::Set(
			minor_sum(txn, refunds_on_day(bounds), transaction::Column::AmountMinor)
				.await?,
		),
		discount_amount_minor: ActiveValue::Set(
			minor_sum(txn, sales_on_day(bounds), transaction::Column::DiscountMinor)
				.await?,
		),
		paying_users: ActiveValue::Set(paying_users(txn, sales_on_day(bounds)).await?),
		tracked_link_hits: ActiveValue::Set(
			count_in_day(
				txn,
				TrackedLinkHits::find(),
				tracked_link_hits::Column::CreatedAt,
				bounds,
			)
			.await?,
		),
	})
}

async fn stripe_acquisitions(
	txn: &DatabaseTransaction,
	bounds: (DateTimeWithTimeZone, DateTimeWithTimeZone),
) -> Result<(i32, i32), DbErr> {
	let stripe = PlayerOwnedCosmetic::find().filter(
		player_owned_cosmetic::Column::AcquiredVia.eq(TransactionProvider::Stripe),
	);

	let total = count_in_day(
		txn,
		stripe.clone(),
		player_owned_cosmetic::Column::AcquiredAt,
		bounds,
	)
	.await?;

	// An acquisition whose transaction never had an amount recorded counts as
	// free; `backfill-stripe` fills those in from Stripe.
	let paid = count_in_day(
		txn,
		stripe
			.inner_join(Transaction)
			.filter(transaction::Column::AmountMinor.gt(0)),
		player_owned_cosmetic::Column::AcquiredAt,
		bounds,
	)
	.await?;

	Ok((paid, total - paid))
}

#[derive(Debug, Default, FromQueryResult)]
struct MoneyTotal {
	total: Option<i64>,
}

fn sales_on_day(
	(start, end): (DateTimeWithTimeZone, DateTimeWithTimeZone),
) -> Select<Transaction> {
	Transaction::find()
		.filter(transaction::Column::CreatedAt.gte(start))
		.filter(transaction::Column::CreatedAt.lt(end))
		.filter(transaction::Column::Status.is_in(SALE_STATUSES))
}

fn refunds_on_day(
	(start, end): (DateTimeWithTimeZone, DateTimeWithTimeZone),
) -> Select<Transaction> {
	Transaction::find()
		.filter(transaction::Column::RefundedAt.gte(start))
		.filter(transaction::Column::RefundedAt.lt(end))
		.filter(transaction::Column::Status.eq(TransactionStatus::Refunded))
}

async fn minor_sum(
	txn: &DatabaseTransaction,
	select: Select<Transaction>,
	column: transaction::Column,
) -> Result<i64, DbErr> {
	Ok(select
		.select_only()
		.column_as(column.sum().cast_as(Alias::new("bigint")), "total")
		.into_model::<MoneyTotal>()
		.one(txn)
		.await?
		.unwrap_or_default()
		.total
		.unwrap_or_default())
}

async fn paying_users(
	txn: &DatabaseTransaction,
	select: Select<Transaction>,
) -> Result<i32, DbErr> {
	let value = select
		.select_only()
		.column_as(
			Expr::expr(Func::count_distinct(Expr::col(
				transaction::Column::PlayerId,
			))),
			"value",
		)
		.into_model::<CountValue>()
		.one(txn)
		.await?
		.unwrap_or_default();

	Ok(value
		.value
		.unwrap_or_default()
		.try_into()
		.unwrap_or(i32::MAX))
}

fn window_start(day: NaiveDate, back: u64) -> NaiveDate {
	day.checked_sub_days(Days::new(back)).unwrap_or(day)
}

async fn active_players(
	txn: &DatabaseTransaction,
	from: NaiveDate,
	to: NaiveDate,
) -> Result<i32, DbErr> {
	let value = DailyPlaytime::find()
		.filter(daily_playtime::Column::Day.gte(from))
		.filter(daily_playtime::Column::Day.lte(to))
		.filter(daily_playtime::Column::TotalSeconds.gt(0))
		.select_only()
		.column_as(
			Expr::expr(Func::count_distinct(Expr::col(
				daily_playtime::Column::PlayerId,
			))),
			"value",
		)
		.into_model::<CountValue>()
		.one(txn)
		.await?
		.unwrap_or_default();

	Ok(value
		.value
		.unwrap_or_default()
		.try_into()
		.unwrap_or(i32::MAX))
}

/// Active on `day` and registered before it.
async fn returning_players(
	txn: &DatabaseTransaction,
	day: NaiveDate,
	day_start: DateTimeWithTimeZone,
) -> Result<i32, DbErr> {
	let value = DailyPlaytime::find()
		.inner_join(User)
		.filter(daily_playtime::Column::Day.eq(day))
		.filter(daily_playtime::Column::TotalSeconds.gt(0))
		.filter(user::Column::CreatedAt.lt(day_start))
		.select_only()
		.column_as(
			Expr::expr(Func::count_distinct(Expr::col((
				daily_playtime::Entity,
				daily_playtime::Column::PlayerId,
			)))),
			"value",
		)
		.into_model::<CountValue>()
		.one(txn)
		.await?
		.unwrap_or_default();

	Ok(value
		.value
		.unwrap_or_default()
		.try_into()
		.unwrap_or(i32::MAX))
}

async fn playtime_totals(
	txn: &DatabaseTransaction,
	day: NaiveDate,
) -> Result<PlaytimeTotals, DbErr> {
	Ok(DailyPlaytime::find()
		.filter(daily_playtime::Column::Day.eq(day))
		.select_only()
		.column_as(
			daily_playtime::Column::TotalSeconds
				.sum()
				.cast_as(Alias::new("bigint")),
			"seconds",
		)
		.column_as(
			daily_playtime::Column::SessionCount
				.sum()
				.cast_as(Alias::new("bigint")),
			"sessions",
		)
		.into_model::<PlaytimeTotals>()
		.one(txn)
		.await?
		.unwrap_or_default())
}
