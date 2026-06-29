use aide::{
	OperationInput, OperationIo,
	axum::{ApiRouter, routing::get_with},
	openapi::SecurityRequirement,
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{FromRequestParts, State},
	http::{StatusCode, request::Parts},
	response::{IntoResponse, Response},
};
use chrono::{Days, Utc};
use entities::{
	daily_playtime, monthly_active_login, player_owned_cosmetic, player_owned_emote,
	prelude::*, sea_orm_active_enums::PlayerRole,
};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, EntityTrait, FromQueryResult, PaginatorTrait as _, QueryFilter,
	QuerySelect, sea_query::Alias,
};
use serde::Serialize;

use crate::{
	api::{
		ApiState,
		account::{AuthenticatedPlayer, OPENAPI_SECURITY_NAME, role_at_least},
	},
	database::current_utc_month,
};

#[derive(Debug)]
pub struct PrivateAnalyticsAuth;

#[derive(Debug, PartialEq, Eq)]
enum PrivateAuthCredential<'a> {
	AdminPassword,
	Bearer(&'a str),
	MissingOrInvalid,
}

fn classify_authorization_header<'a>(
	header: Option<&'a str>,
	admin_password: &str,
) -> PrivateAuthCredential<'a> {
	match header {
		Some(value) if value == admin_password => PrivateAuthCredential::AdminPassword,
		Some(value) => value
			.strip_prefix("Bearer ")
			.map(PrivateAuthCredential::Bearer)
			.unwrap_or(PrivateAuthCredential::MissingOrInvalid),
		None => PrivateAuthCredential::MissingOrInvalid,
	}
}

fn is_admin_role(role: &PlayerRole) -> bool {
	role_at_least(role, &PlayerRole::Admin)
}

impl OperationInput for PrivateAnalyticsAuth {
	fn operation_input(
		_ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.security.extend([
			SecurityRequirement::from([("Admin Password".to_string(), Vec::new())]),
			SecurityRequirement::from([(OPENAPI_SECURITY_NAME.to_string(), Vec::new())]),
		]);
	}
}

impl FromRequestParts<ApiState> for PrivateAnalyticsAuth {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		let auth_header = parts
			.headers
			.get("Authorization")
			.and_then(|h| h.to_str().ok());

		match classify_authorization_header(auth_header, &state.admin_password) {
			PrivateAuthCredential::AdminPassword => Ok(Self),
			PrivateAuthCredential::Bearer(_) => {
				let player = AuthenticatedPlayer::from_request_parts(parts, state)
					.await?
					.0;
				if is_admin_role(&player.role) {
					Ok(Self)
				} else {
					Err((
						StatusCode::FORBIDDEN,
						"Authenticated player does not have permission",
					)
						.into_response())
				}
			}
			PrivateAuthCredential::MissingOrInvalid => Err((
				StatusCode::UNAUTHORIZED,
				"Invalid or missing analytics authorization",
			)
				.into_response()),
		}
	}
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum AnalyticsError {
	#[error("Unable to query analytics data: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for AnalyticsError {
	fn into_response(self) -> axum::response::Response {
		(StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsOverview")
		.summary("Get private analytics overview")
		.description(
			"Returns private aggregate analytics for users, MAU, owned items, and playtime.",
		)
		.tag("analytics")
}

#[derive(Debug, Serialize, JsonSchema)]
struct AnalyticsOverviewResponse {
	total_users: i64,
	monthly_active_users: i64,
	owned_items_per_user: OwnedItemsPerUser,
	playtime: Playtime,
}

#[derive(Debug, Serialize, JsonSchema)]
struct Playtime {
	total_seconds: i64,
	average_seconds_per_user: f64,
	last_30d_seconds: i64,
	total_sessions: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct OwnedItemsPerUser {
	total_owned_items: i64,
	average_per_user: f64,
	users_with_any: i64,
	distribution: OwnedItemsDistribution,
}

#[derive(Debug, Serialize, JsonSchema)]
struct OwnedItemsDistribution {
	#[serde(rename = "0")]
	zero: i64,
	#[serde(rename = "1")]
	one: i64,
	#[serde(rename = "2_5")]
	two_5: i64,
	#[serde(rename = "6_10")]
	six_10: i64,
	#[serde(rename = "11_plus")]
	eleven_plus: i64,
}

#[derive(Debug, FromQueryResult)]
struct UserCounts {
	total_users: i64,
	monthly_active_users: i64,
}

#[derive(Debug, FromQueryResult)]
struct OwnedItemsCounts {
	total_owned_items: i64,
	users_with_any: i64,
	zero: i64,
	one: i64,
	two_5: i64,
	six_10: i64,
	eleven_plus: i64,
}

#[derive(Debug, FromQueryResult)]
struct PlayerOwnedCount {
	player_id: i32,
	count: i64,
}

#[derive(Debug, Default, FromQueryResult)]
struct PlaytimeAggregate {
	total_seconds: Option<i64>,
	total_sessions: Option<i64>,
}

impl OwnedItemsCounts {
	fn from_player_counts(
		total_users: i64,
		player_counts: impl IntoIterator<Item = i64>,
	) -> Self {
		let mut counts = Self {
			total_owned_items: 0,
			users_with_any: 0,
			zero: total_users,
			one: 0,
			two_5: 0,
			six_10: 0,
			eleven_plus: 0,
		};

		for owned_count in player_counts {
			counts.total_owned_items += owned_count;

			if owned_count > 0 {
				counts.users_with_any += 1;
				counts.zero -= 1;
			}

			match owned_count {
				1 => counts.one += 1,
				2..=5 => counts.two_5 += 1,
				6..=10 => counts.six_10 += 1,
				11.. => counts.eleven_plus += 1,
				_ => {}
			}
		}

		counts
	}
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/analytics/overview",
		get_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
) -> Result<Json<AnalyticsOverviewResponse>, AnalyticsError> {
	let user_counts = UserCounts {
		total_users: User::find().count(&state.database).await? as i64,
		monthly_active_users: MonthlyActiveLogin::find()
			.filter(monthly_active_login::Column::Month.eq(current_utc_month()))
			.count(&state.database)
			.await? as i64,
	};

	let cosmetic_counts = PlayerOwnedCosmetic::find()
		.select_only()
		.column(player_owned_cosmetic::Column::PlayerId)
		.column_as(player_owned_cosmetic::Column::CosmeticId.count(), "count")
		.group_by(player_owned_cosmetic::Column::PlayerId)
		.into_model::<PlayerOwnedCount>()
		.all(&state.database)
		.await?;
	let emote_counts = PlayerOwnedEmote::find()
		.select_only()
		.column(player_owned_emote::Column::PlayerId)
		.column_as(player_owned_emote::Column::EmoteId.count(), "count")
		.group_by(player_owned_emote::Column::PlayerId)
		.into_model::<PlayerOwnedCount>()
		.all(&state.database)
		.await?;

	let mut owned_by_player = std::collections::HashMap::new();
	for count in cosmetic_counts.into_iter().chain(emote_counts) {
		*owned_by_player.entry(count.player_id).or_insert(0) += count.count;
	}
	let owned_counts = OwnedItemsCounts::from_player_counts(
		user_counts.total_users,
		owned_by_player.into_values(),
	);

	let average_per_user = if user_counts.total_users == 0 {
		0.0
	} else {
		owned_counts.total_owned_items as f64 / user_counts.total_users as f64
	};

	let playtime_totals = DailyPlaytime::find()
		.select_only()
		.column_as(
			daily_playtime::Column::TotalSeconds
				.sum()
				.cast_as(Alias::new("bigint")),
			"total_seconds",
		)
		.column_as(
			daily_playtime::Column::SessionCount
				.sum()
				.cast_as(Alias::new("bigint")),
			"total_sessions",
		)
		.into_model::<PlaytimeAggregate>()
		.one(&state.database)
		.await?
		.unwrap_or_default();

	let thirty_days_ago = (Utc::now() - Days::new(30)).date_naive();
	let last_30d_seconds = DailyPlaytime::find()
		.select_only()
		.column_as(
			daily_playtime::Column::TotalSeconds
				.sum()
				.cast_as(Alias::new("bigint")),
			"total_seconds",
		)
		.filter(daily_playtime::Column::Day.gte(thirty_days_ago))
		.into_model::<PlaytimeAggregate>()
		.one(&state.database)
		.await?
		.unwrap_or_default()
		.total_seconds
		.unwrap_or(0);

	let total_playtime_seconds = playtime_totals.total_seconds.unwrap_or(0);
	let average_seconds_per_user = if user_counts.total_users == 0 {
		0.0
	} else {
		total_playtime_seconds as f64 / user_counts.total_users as f64
	};

	Ok(Json(AnalyticsOverviewResponse {
		total_users: user_counts.total_users,
		monthly_active_users: user_counts.monthly_active_users,
		playtime: Playtime {
			total_seconds: total_playtime_seconds,
			average_seconds_per_user,
			last_30d_seconds,
			total_sessions: playtime_totals.total_sessions.unwrap_or(0),
		},
		owned_items_per_user: OwnedItemsPerUser {
			total_owned_items: owned_counts.total_owned_items,
			average_per_user,
			users_with_any: owned_counts.users_with_any,
			distribution: OwnedItemsDistribution {
				zero: owned_counts.zero,
				one: owned_counts.one,
				two_5: owned_counts.two_5,
				six_10: owned_counts.six_10,
				eleven_plus: owned_counts.eleven_plus,
			},
		},
	}))
}

#[cfg(test)]
mod tests {
	use entities::sea_orm_active_enums::PlayerRole;

	use super::{PrivateAuthCredential, classify_authorization_header, is_admin_role};

	#[test]
	fn auth_header_accepts_admin_password() {
		assert_eq!(
			classify_authorization_header(Some("secret"), "secret"),
			PrivateAuthCredential::AdminPassword
		);
	}

	#[test]
	fn auth_header_accepts_bearer_token() {
		assert_eq!(
			classify_authorization_header(Some("Bearer token"), "secret"),
			PrivateAuthCredential::Bearer("token")
		);
	}

	#[test]
	fn auth_header_rejects_missing_or_invalid_values() {
		assert_eq!(
			classify_authorization_header(None, "secret"),
			PrivateAuthCredential::MissingOrInvalid
		);
		assert_eq!(
			classify_authorization_header(Some("token"), "secret"),
			PrivateAuthCredential::MissingOrInvalid
		);
	}

	#[test]
	fn only_admin_role_can_use_bearer_analytics_auth() {
		assert!(is_admin_role(&PlayerRole::Admin));
		assert!(!is_admin_role(&PlayerRole::Moderator));
		assert!(!is_admin_role(&PlayerRole::Player));
	}
}
