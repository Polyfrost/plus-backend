//! Admin dashboard sign-in. The browser does the Microsoft and Xbox Live
//! steps itself; Mojang's endpoints send no CORS headers, so the last hop -
//! trading the XSTS token for a Minecraft profile - happens here.

use std::time::Duration;

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{FromRequestParts, State},
	http::{HeaderValue, StatusCode, header, request::Parts},
	response::{IntoResponse, Response},
};
use schemars::JsonSchema;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
	api::{
		ApiState, permissions,
		rate_limit::ClientAddress,
		v0::account::{SESSION_COOKIE, TokenAccess, from_dashboard, mint_token},
	},
	database::DatabaseUserExt,
};

const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 12);
const MOJANG_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum DashboardError {
	#[error("Sign-in is only accepted from the admin dashboard")]
	WrongOrigin,
	#[error("Minecraft rejected that Xbox Live token")]
	Rejected,
	#[error("That Microsoft account does not own Minecraft")]
	NoProfile,
	#[error("Your account has no dashboard permissions")]
	NoPermissions,
	#[error("Your account is blacklisted")]
	Blacklisted,
	#[error("Too many sign-in attempts, try again in a minute")]
	RateLimited,
	#[error("Unable to reach Minecraft services: {0}")]
	Mojang(#[from] reqwest::Error),
	#[error("Unable to construct session token: {0}")]
	Token(#[from] pasetors::errors::Error),
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for DashboardError {
	fn into_response(self) -> Response {
		crate::api::error_response(
			match self {
				Self::WrongOrigin | Self::NoPermissions | Self::Blacklisted => {
					StatusCode::FORBIDDEN
				}
				Self::Rejected | Self::NoProfile => StatusCode::UNAUTHORIZED,
				Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
				Self::Mojang(_) => StatusCode::BAD_GATEWAY,
				Self::Token(_) | Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LoginRequest {
	/// The user hash (`uhs`) from the XSTS response.
	user_hash: String,
	/// An XSTS token for the `rp://api.minecraftservices.com/` relying party.
	xsts_token: String,
}

#[derive(Deserialize)]
struct MinecraftToken {
	access_token: String,
}

#[derive(Deserialize)]
struct MinecraftProfile {
	id: Uuid,
	name: String,
}

/// Requests from a page other than the dashboard, which must not be able to
/// set or clear its session.
struct DashboardOrigin;

impl aide::OperationInput for DashboardOrigin {}

impl FromRequestParts<ApiState> for DashboardOrigin {
	type Rejection = DashboardError;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		from_dashboard(parts, state)
			.then_some(Self)
			.ok_or(DashboardError::WrongOrigin)
	}
}

fn login_doc(op: TransformOperation) -> TransformOperation {
	op.id("dashboardLogin")
		.summary("Sign in to the admin dashboard")
		.description(
			"Trades an XSTS token for the caller's Minecraft profile and sets \
			 an HttpOnly session cookie. Only players holding at least one \
			 permission may sign in, and only from a dashboard origin.",
		)
		.tag("dashboard")
}

fn logout_doc(op: TransformOperation) -> TransformOperation {
	op.id("dashboardLogout")
		.summary("Sign out of the admin dashboard")
		.tag("dashboard")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	let router = ApiRouter::new()
		.api_route("/dashboard/login", post_with(self::login, self::login_doc))
		.api_route("/dashboard/logout", post_with(self::logout, self::logout_doc));

	#[cfg(debug_assertions)]
	let router = router.api_route(
		"/dashboard/dev-login",
		post_with(dev::dev_login, |op| {
			op.id("dashboardDevLogin")
				.summary("Debug builds only: sign in as any player")
				.tag("dashboard")
		}),
	);

	router
}

fn session_cookie(token: &str, max_age: Duration) -> HeaderValue {
	HeaderValue::from_str(&format!(
		"{SESSION_COOKIE}={token}; Max-Age={}; Path=/; HttpOnly; Secure; SameSite=Strict",
		max_age.as_secs()
	))
	.expect("paseto tokens are header safe")
}

/// Signs `uuid` in, provided they hold any permission at all.
async fn start_session(
	state: &ApiState,
	uuid: Uuid,
	username: Option<&str>,
) -> Result<String, DashboardError> {
	let player = entities::prelude::User::get_or_create(&state.database, uuid).await?;
	if player.blacklisted {
		return Err(DashboardError::Blacklisted);
	}
	if let Some(username) = username {
		entities::prelude::User::set_username(&state.database, player.id, username).await?;
	}

	let roles = permissions::roles_of(&state.database, player.id).await?;
	if permissions::union(&roles) == 0 {
		return Err(DashboardError::NoPermissions);
	}

	tracing::info!(%uuid, "Signed in to the dashboard");
	Ok(mint_token(state, uuid, false, SESSION_TTL, TokenAccess::Dashboard)?)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn login(
	State(state): State<ApiState>,
	_origin: DashboardOrigin,
	ClientAddress(address): ClientAddress,
	Json(body): Json<LoginRequest>,
) -> Result<Response, DashboardError> {
	if let Some(address) = address
		&& state
			.dashboard_login_limit
			.check(crate::api::rate_limit::address_key(address))
			.await
			.is_some()
	{
		return Err(DashboardError::RateLimited);
	}

	let response = state
		.client
		.post("https://api.minecraftservices.com/authentication/login_with_xbox")
		.timeout(MOJANG_TIMEOUT)
		.json(&serde_json::json!({
			"identityToken": format!("XBL3.0 x={};{}", body.user_hash, body.xsts_token),
		}))
		.send()
		.await?;
	if response.status().is_client_error() {
		return Err(DashboardError::Rejected);
	}
	let minecraft: MinecraftToken = response.error_for_status()?.json().await?;

	let response = state
		.client
		.get("https://api.minecraftservices.com/minecraft/profile")
		.timeout(MOJANG_TIMEOUT)
		.bearer_auth(&minecraft.access_token)
		.send()
		.await?;
	// 404 when the account has never bought the game.
	if response.status() == StatusCode::NOT_FOUND {
		return Err(DashboardError::NoProfile);
	}
	let profile: MinecraftProfile = response.error_for_status()?.json().await?;

	let token = start_session(&state, profile.id, Some(&profile.name)).await?;

	Ok((
		StatusCode::NO_CONTENT,
		[(header::SET_COOKIE, session_cookie(&token, SESSION_TTL))],
	)
		.into_response())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn logout(_origin: DashboardOrigin) -> Response {
	(
		StatusCode::NO_CONTENT,
		[(header::SET_COOKIE, session_cookie("", Duration::ZERO))],
	)
		.into_response()
}

/// Debug builds only: lets local seed scripts and a local dashboard sign in
/// without Microsoft.
#[cfg(debug_assertions)]
mod dev {
	use super::*;

	/// The socket's own peer address, which no forwarding header can change.
	pub(super) struct Peer(std::net::SocketAddr);

	impl aide::OperationInput for Peer {}

	impl FromRequestParts<ApiState> for Peer {
		type Rejection = DashboardError;

		async fn from_request_parts(
			parts: &mut Parts,
			_state: &ApiState,
		) -> Result<Self, Self::Rejection> {
			parts
				.extensions
				.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
				.map(|info| Self(info.0))
				.ok_or(DashboardError::WrongOrigin)
		}
	}

	#[derive(Debug, Deserialize, JsonSchema)]
	pub(super) struct DevLoginRequest {
		player: Uuid,
		/// Mint a plain player token, as `/login` gives the game, to test that
		/// those never carry permissions.
		#[serde(default)]
		game: bool,
	}

	/// Compiled out of release builds, and even then only answers this machine:
	/// debug builds bind every interface. Also returns the token for use as a
	/// bearer token, since scripts have no cookie jar.
	#[tracing::instrument(level = "debug", skip(state))]
	pub(super) async fn dev_login(
		State(state): State<ApiState>,
		Peer(peer): Peer,
		Json(body): Json<DevLoginRequest>,
	) -> Result<Response, DashboardError> {
		if !peer.ip().to_canonical().is_loopback() {
			return Err(DashboardError::WrongOrigin);
		}
		let token = if body.game {
			mint_token(&state, body.player, true, SESSION_TTL, TokenAccess::Player)?
		} else {
			start_session(&state, body.player, None).await?
		};

		Ok((
			[(header::SET_COOKIE, session_cookie(&token, SESSION_TTL))],
			Json(serde_json::json!({ "token": token })),
		)
			.into_response())
	}
}
