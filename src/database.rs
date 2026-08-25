use chrono::{DateTime, Days, NaiveDate, Utc};
use entities::{
	cosmetic_ownership_event, daily_playtime, monthly_active_login, player_client_info,
	prelude::*,
	sea_orm_active_enums::{OwnershipEventKind, TransactionProvider, TransactionStatus},
	transaction, user,
};
use sea_orm::{
	ActiveValue, DbErr, EntityTrait, QueryFilter, Set,
	prelude::*,
	sea_query::{Alias, Expr, OnConflict},
};
use uuid::Uuid;

use crate::utils::time::current_utc_month;

pub(crate) trait DatabaseUserExt {
	/// Gets a [user::Model] given a specific Minecraft UUID, or else inserts a
	/// new user into the database.
	async fn get_or_create(
		db: &impl ConnectionTrait,
		minecraft_uuid: Uuid,
	) -> Result<user::Model, DbErr>;

	async fn set_username(
		db: &impl ConnectionTrait,
		player_id: i32,
		username: &str,
	) -> Result<(), DbErr>;
}

/// What Stripe reported for a checkout session, in the currency's minor units.
#[derive(Debug, Default, Clone)]
pub(crate) struct StripeCharge {
	pub amount_minor: Option<i64>,
	pub currency: Option<String>,
	pub discount_minor: Option<i64>,
}

pub(crate) trait DatabaseTransactionExt {
	async fn get_or_create_stripe(
		db: &impl ConnectionTrait,
		player_id: i32,
		buyer_id: Option<i32>,
		transaction_id: &str,
		raw_metadata: serde_json::Value,
		charged: StripeCharge,
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

	async fn set_username(
		db: &impl ConnectionTrait,
		player_id: i32,
		username: &str,
	) -> Result<(), DbErr> {
		User::update_many()
			.col_expr(user::Column::Username, Expr::value(username.to_string()))
			.filter(user::Column::Id.eq(player_id))
			.filter(
				user::Column::Username
					.ne(username)
					.or(user::Column::Username.is_null()),
			)
			.exec(db)
			.await?;
		Ok(())
	}
}

impl DatabaseTransactionExt for Transaction {
	async fn get_or_create_stripe(
		db: &impl ConnectionTrait,
		player_id: i32,
		buyer_id: Option<i32>,
		stripe_payment_id: &str,
		raw_metadata: serde_json::Value,
		charged: StripeCharge,
	) -> Result<transaction::Model, DbErr> {
		if let Some(existing) = Transaction::find()
			.filter(transaction::Column::Provider.eq(TransactionProvider::Stripe))
			.filter(transaction::Column::StripePaymentId.eq(stripe_payment_id))
			.one(db)
			.await?
		{
			if existing.amount_minor.is_some() || charged.amount_minor.is_none() {
				return Ok(existing);
			}

			let mut update: transaction::ActiveModel = existing.into();
			update.amount_minor = Set(charged.amount_minor);
			update.currency = Set(charged.currency);
			update.discount_minor = Set(charged.discount_minor);

			return update.update(db).await;
		}

		Transaction::insert(transaction::ActiveModel {
			player_id: ActiveValue::Set(player_id),
			provider: ActiveValue::Set(TransactionProvider::Stripe),
			stripe_payment_id: ActiveValue::Set(Some(stripe_payment_id.to_string())),
			status: ActiveValue::Set(TransactionStatus::Completed),
			buyer: ActiveValue::Set(buyer_id),
			raw_metadata: ActiveValue::Set(raw_metadata),
			amount_minor: ActiveValue::Set(charged.amount_minor),
			currency: ActiveValue::Set(charged.currency),
			discount_minor: ActiveValue::Set(charged.discount_minor),
			..Default::default()
		})
		.exec_with_returning(db)
		.await
	}
}

pub(crate) async fn record_monthly_active_login(
	db: &impl ConnectionTrait,
	player_id: i32,
) -> Result<(), DbErr> {
	MonthlyActiveLogin::insert(monthly_active_login::ActiveModel {
		player_id: Set(player_id),
		month: Set(current_utc_month()),
		first_login_at: ActiveValue::NotSet,
		last_login_at: ActiveValue::NotSet,
		login_count: Set(1),
	})
	.on_conflict(
		OnConflict::columns([
			monthly_active_login::Column::PlayerId,
			monthly_active_login::Column::Month,
		])
		.value(
			monthly_active_login::Column::LastLoginAt,
			Expr::current_timestamp(),
		)
		.value(
			monthly_active_login::Column::LoginCount,
			Expr::col((
				monthly_active_login::Entity,
				monthly_active_login::Column::LoginCount,
			))
			.add(1),
		)
		.to_owned(),
	)
	.exec_without_returning(db)
	.await?;

	Ok(())
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ClientInfo {
	pub client_version: Option<String>,
	pub minecraft_version: Option<String>,
	pub loader: Option<String>,
	pub os: Option<String>,
	pub os_version: Option<String>,
	pub java_version: Option<String>,
}

impl ClientInfo {
	fn is_empty(&self) -> bool {
		self.client_version.is_none()
			&& self.minecraft_version.is_none()
			&& self.loader.is_none()
			&& self.os.is_none()
			&& self.os_version.is_none()
			&& self.java_version.is_none()
	}
}

pub(crate) async fn record_client_info(
	db: &impl ConnectionTrait,
	player_id: i32,
	info: ClientInfo,
) -> Result<(), DbErr> {
	if info.is_empty() {
		return Ok(());
	}

	PlayerClientInfo::insert(player_client_info::ActiveModel {
		player_id: Set(player_id),
		client_version: Set(info.client_version),
		minecraft_version: Set(info.minecraft_version),
		loader: Set(info.loader),
		os: Set(info.os),
		os_version: Set(info.os_version),
		java_version: Set(info.java_version),
		..Default::default()
	})
	.on_conflict(
		OnConflict::column(player_client_info::Column::PlayerId)
			.update_columns([
				player_client_info::Column::ClientVersion,
				player_client_info::Column::MinecraftVersion,
				player_client_info::Column::Loader,
				player_client_info::Column::Os,
				player_client_info::Column::OsVersion,
				player_client_info::Column::JavaVersion,
			])
			.value(
				player_client_info::Column::LastSeenAt,
				Expr::current_timestamp(),
			)
			.to_owned(),
	)
	.exec_without_returning(db)
	.await?;

	Ok(())
}

pub(crate) async fn record_ownership_events(
	db: &impl ConnectionTrait,
	player_id: i32,
	cosmetic_ids: &[i32],
	kind: OwnershipEventKind,
	provider: TransactionProvider,
	transaction_id: Option<i32>,
) -> Result<(), DbErr> {
	if cosmetic_ids.is_empty() {
		return Ok(());
	}

	CosmeticOwnershipEvent::insert_many(cosmetic_ids.iter().map(|&cosmetic_id| {
		cosmetic_ownership_event::ActiveModel {
			player_id: Set(player_id),
			cosmetic_id: Set(cosmetic_id),
			kind: Set(kind.clone()),
			provider: Set(provider.clone()),
			transaction_id: Set(transaction_id),
			..Default::default()
		}
	}))
	.exec_without_returning(db)
	.await?;

	Ok(())
}

/// Splits each `[from, to)` window at UTC midnight and sums the seconds falling
/// on each day, merging windows that land on the same `(player_id, day)`.
pub(crate) fn coalesce_playtime_windows(
	windows: &[(i32, DateTime<Utc>, DateTime<Utc>)],
) -> Vec<(i32, NaiveDate, i64)> {
	let mut totals: std::collections::HashMap<(i32, NaiveDate), i64> =
		std::collections::HashMap::new();

	for &(player_id, from, to) in windows {
		let mut cursor = from;
		while cursor < to {
			let day = cursor.date_naive();
			let next_midnight = (day + Days::new(1))
				.and_hms_opt(0, 0, 0)
				.expect("midnight is a valid time")
				.and_utc();
			let segment_end = next_midnight.min(to);

			*totals.entry((player_id, day)).or_default() +=
				(segment_end - cursor).num_seconds().max(0);
			cursor = segment_end;
		}
	}

	let mut buckets: Vec<(i32, NaiveDate, i64)> = totals
		.into_iter()
		.map(|((player_id, day), seconds)| (player_id, day, seconds))
		.collect();

	// Deterministic order keeps the row locks in a consistent sequence.
	buckets.sort_unstable();

	buckets
}

/// Commits every pending playtime window in one statement.
pub(crate) async fn flush_playtime_windows(
	db: &impl ConnectionTrait,
	windows: &[(i32, DateTime<Utc>, DateTime<Utc>)],
) -> Result<(), DbErr> {
	let buckets = coalesce_playtime_windows(windows);
	if buckets.is_empty() {
		return Ok(());
	}

	let rows = buckets.into_iter().map(|(player_id, day, seconds)| {
		daily_playtime::ActiveModel {
			player_id: Set(player_id),
			day: Set(day),
			total_seconds: Set(seconds),
			// Only the `end_session` path may bump this.
			session_count: Set(0),
		}
	});

	DailyPlaytime::insert_many(rows)
		.on_conflict(
			OnConflict::columns([
				daily_playtime::Column::PlayerId,
				daily_playtime::Column::Day,
			])
			.value(
				daily_playtime::Column::TotalSeconds,
				Expr::col((daily_playtime::Entity, daily_playtime::Column::TotalSeconds))
					.add(Expr::col((
						Alias::new("excluded"),
						daily_playtime::Column::TotalSeconds,
					))),
			)
			.to_owned(),
		)
		.exec_without_returning(db)
		.await?;

	Ok(())
}

pub(crate) async fn accrue_playtime(
	db: &impl ConnectionTrait,
	player_id: i32,
	from: DateTime<Utc>,
	to: DateTime<Utc>,
	end_session: bool,
) -> Result<(), DbErr> {
	let mut cursor = from;
	while cursor < to {
		let day = cursor.date_naive();
		let next_midnight = (day + Days::new(1))
			.and_hms_opt(0, 0, 0)
			.expect("midnight is a valid time")
			.and_utc();
		let segment_end = next_midnight.min(to);
		let seconds = (segment_end - cursor).num_seconds().max(0);
		let is_final = segment_end >= to;
		upsert_daily_playtime(db, player_id, day, seconds, end_session && is_final)
			.await?;
		cursor = segment_end;
	}

	if end_session && from >= to {
		upsert_daily_playtime(db, player_id, to.date_naive(), 0, true).await?;
	}

	Ok(())
}

async fn upsert_daily_playtime(
	db: &impl ConnectionTrait,
	player_id: i32,
	day: NaiveDate,
	seconds: i64,
	increment_session: bool,
) -> Result<(), DbErr> {
	let session_delta = i32::from(increment_session);

	DailyPlaytime::insert(daily_playtime::ActiveModel {
		player_id: Set(player_id),
		day: Set(day),
		total_seconds: Set(seconds),
		session_count: Set(session_delta),
	})
	.on_conflict(
		OnConflict::columns([
			daily_playtime::Column::PlayerId,
			daily_playtime::Column::Day,
		])
		.value(
			daily_playtime::Column::TotalSeconds,
			Expr::col((daily_playtime::Entity, daily_playtime::Column::TotalSeconds))
				.add(seconds),
		)
		.value(
			daily_playtime::Column::SessionCount,
			Expr::col((daily_playtime::Entity, daily_playtime::Column::SessionCount))
				.add(session_delta),
		)
		.to_owned(),
	)
	.exec_without_returning(db)
	.await?;

	Ok(())
}
