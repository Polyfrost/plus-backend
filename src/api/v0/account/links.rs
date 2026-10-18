use std::{fmt, time::Duration};

use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, Query, State},
	http::StatusCode,
	response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, FixedOffset};
use moka::future::Cache;
use rand::RngCore;
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, SqlErr, sea_query::OnConflict};
use serde::{Deserialize, Serialize};

use crate::{
	api::{ApiState, v0::account::AuthenticatedPlayer},
	commands::ServeArgs,
};
use entities::{linked_account, prelude::*, sea_orm_active_enums::LinkProvider};

const LINK_STATE_TTL: Duration = Duration::from_secs(10 * 60);
const DISCORD_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct DiscordOAuth {
	client_id: String,
	client_secret: String,
	redirect_uri: String,
}

impl fmt::Debug for DiscordOAuth {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DiscordOAuth")
			.field("client_id", &self.client_id)
			.finish_non_exhaustive()
	}
}

impl DiscordOAuth {
	pub(crate) fn from_args(args: &ServeArgs) -> Option<Self> {
		(!args.discord_client_id.is_empty()).then(|| Self {
			client_id: args.discord_client_id.clone(),
			client_secret: args.discord_client_secret.clone(),
			redirect_uri: args.discord_redirect_uri.clone(),
		})
	}
}

/// Pending OAuth `state` values, each mapped to the user who started the link.
pub(crate) fn new_link_state_cache() -> Cache<String, i32> {
	Cache::builder().time_to_live(LINK_STATE_TTL).build()
}

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum LinkError {
	#[error("Linking this provider is not configured")]
	NotConfigured,
	#[error("This link request has expired, start linking again")]
	InvalidState,
	#[error("Linking was cancelled")]
	Cancelled,
	#[error("That account is already linked to another player")]
	AlreadyLinked,
	#[error("Unable to reach the provider: {0}")]
	Provider(#[from] reqwest::Error),
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for LinkError {
	fn into_response(self) -> Response {
		crate::api::error_response(
			match self {
				Self::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
				Self::InvalidState | Self::Cancelled => StatusCode::BAD_REQUEST,
				Self::AlreadyLinked => StatusCode::CONFLICT,
				Self::Provider(_) => StatusCode::BAD_GATEWAY,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Serialize, JsonSchema)]
struct LinkedAccountInfo {
	provider: LinkProvider,
	provider_user_id: String,
	username: Option<String>,
	linked_at: DateTime<FixedOffset>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct StartLinkResponse {
	/// Open in the player's browser to authorize the link.
	url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CallbackQuery {
	/// Absent when the player declined on Discord's consent screen.
	code: Option<String>,
	state: String,
}

#[derive(Deserialize)]
struct DiscordToken {
	access_token: String,
}

#[derive(Deserialize)]
struct DiscordUser {
	id: String,
	username: String,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listLinkedAccounts")
		.summary("List the caller's linked external accounts")
		.tag("account")
}

fn start_discord_doc(op: TransformOperation) -> TransformOperation {
	op.id("startDiscordLink")
		.summary("Start linking a Discord account")
		.description(
			"Returns a Discord authorization url to open in the player's \
			 browser. Discord redirects back to the callback, which completes \
			 the link. Linking again replaces the existing Discord account.",
		)
		.tag("account")
}

fn discord_callback_doc(op: TransformOperation) -> TransformOperation {
	op.id("discordLinkCallback")
		.summary("Discord OAuth2 redirect target")
		.description("Visited by the player's browser, not called directly.")
		.tag("account")
}

fn unlink_doc(op: TransformOperation) -> TransformOperation {
	op.id("unlinkAccount")
		.summary("Unlink the caller's account for a provider")
		.tag("account")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/links", get_with(self::list, self::list_doc))
		.api_route(
			"/links/discord",
			post_with(self::start_discord, self::start_discord_doc),
		)
		.api_route(
			"/links/discord/callback",
			get_with(self::discord_callback, self::discord_callback_doc),
		)
		.api_route("/links/{provider}", delete_with(self::unlink, self::unlink_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<Vec<LinkedAccountInfo>>, LinkError> {
	let links = LinkedAccount::find()
		.filter(linked_account::Column::UserId.eq(player.id))
		.all(&state.database)
		.await?;

	Ok(Json(
		links
			.into_iter()
			.map(|link| LinkedAccountInfo {
				provider: link.provider,
				provider_user_id: link.provider_user_id,
				username: link.username,
				linked_at: link.linked_at,
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn start_discord(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<StartLinkResponse>, LinkError> {
	let discord = state.discord.as_ref().ok_or(LinkError::NotConfigured)?;

	let mut bytes = [0u8; 32];
	rand::rng().fill_bytes(&mut bytes);
	let link_state = URL_SAFE_NO_PAD.encode(bytes);
	state.link_states.insert(link_state.clone(), player.id).await;

	// No `prompt=none`: the consent screen is what stops someone completing a
	// link they were sent, which would tie their Discord to the sender.
	let url = reqwest::Url::parse_with_params(
		"https://discord.com/oauth2/authorize",
		[
			("client_id", discord.client_id.as_str()),
			("response_type", "code"),
			("scope", "identify"),
			("redirect_uri", discord.redirect_uri.as_str()),
			("state", link_state.as_str()),
		],
	)
	.expect("static Discord authorize url is valid");

	Ok(Json(StartLinkResponse { url: url.into() }))
}

#[tracing::instrument(level = "debug", skip(state, query))]
async fn discord_callback(
	State(state): State<ApiState>,
	Query(query): Query<CallbackQuery>,
) -> Result<&'static str, LinkError> {
	let discord = state.discord.as_ref().ok_or(LinkError::NotConfigured)?;
	let user_id = state
		.link_states
		.remove(&query.state)
		.await
		.ok_or(LinkError::InvalidState)?;
	let code = query.code.ok_or(LinkError::Cancelled)?;

	let token: DiscordToken = state
		.client
		.post("https://discord.com/api/oauth2/token")
		.timeout(DISCORD_TIMEOUT)
		.basic_auth(&discord.client_id, Some(&discord.client_secret))
		.form(&[
			("grant_type", "authorization_code"),
			("code", code.as_str()),
			("redirect_uri", discord.redirect_uri.as_str()),
		])
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let discord_user: DiscordUser = state
		.client
		.get("https://discord.com/api/users/@me")
		.timeout(DISCORD_TIMEOUT)
		.bearer_auth(&token.access_token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	link(
		&state,
		user_id,
		LinkProvider::Discord,
		discord_user.id,
		Some(discord_user.username),
	)
	.await?;

	Ok("Your Discord account is now linked. You can close this tab.")
}

#[tracing::instrument(level = "debug", skip(state))]
async fn unlink(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(provider): Path<LinkProvider>,
) -> Result<StatusCode, LinkError> {
	LinkedAccount::delete_by_id((player.id, provider))
		.exec(&state.database)
		.await?;

	Ok(StatusCode::NO_CONTENT)
}

/// Links the provider account to the user, replacing any account they already
/// linked for that provider.
async fn link(
	state: &ApiState,
	user_id: i32,
	provider: LinkProvider,
	provider_user_id: String,
	username: Option<String>,
) -> Result<(), LinkError> {
	let result = LinkedAccount::insert(linked_account::ActiveModel {
		user_id: Set(user_id),
		provider: Set(provider),
		provider_user_id: Set(provider_user_id),
		username: Set(username),
		linked_at: Set(chrono::Utc::now().fixed_offset()),
	})
	.on_conflict(
		OnConflict::columns([
			linked_account::Column::UserId,
			linked_account::Column::Provider,
		])
		.update_columns([
			linked_account::Column::ProviderUserId,
			linked_account::Column::Username,
			linked_account::Column::LinkedAt,
		])
		.to_owned(),
	)
	.exec_without_returning(&state.database)
	.await;

	match result {
		Ok(_) => Ok(()),
		Err(error) if matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
			Err(LinkError::AlreadyLinked)
		}
		Err(error) => Err(error.into()),
	}
}
