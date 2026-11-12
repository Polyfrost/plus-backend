mod link_puid;
pub(crate) mod links;
mod login;
pub(super) mod mods;
mod status;

use aide::{OperationInput, axum::ApiRouter, openapi::SecurityRequirement};
use axum::{
	extract::FromRequestParts,
	http::request::Parts,
	response::{IntoResponse, Response},
};
use http::StatusCode;
use pasetors::{
	Local,
	claims::{Claims, ClaimsValidationRules},
	local,
	token::UntrustedToken,
	version4::V4,
};
use reqwest::header::AUTHORIZATION;
use uuid::Uuid;

use crate::{api::ApiState, database::DatabaseUserExt};
use entities::{prelude::*, user};

pub const OPENAPI_SECURITY_NAME: &str = "Bearer Token";
pub const PASETO_IMPLICIT_ASSERT: Option<&[u8]> = Some(b"plus-backend");

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(login::router())
		.merge(link_puid::router())
		.merge(links::router())
		.merge(mods::router())
		.merge(status::router())
}

#[derive(Debug)]
pub struct AuthenticationExtractor(pub Uuid);
#[derive(Debug)]
pub struct OptionalAuthenticationExtractor(pub Option<Uuid>);
#[derive(Debug)]
pub struct AuthenticatedPlayer(pub user::Model);
/// What kind of client a token was issued to, as claimed at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientKind {
	Game,
	#[default]
	Other,
}

impl OperationInput for AuthenticationExtractor {
	fn operation_input(
		_ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.security.push(SecurityRequirement::from([(
			OPENAPI_SECURITY_NAME.to_string(),
			Vec::new(),
		)]));
	}
}

impl OperationInput for OptionalAuthenticationExtractor {
	fn operation_input(
		_ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		operation.security.extend([
			SecurityRequirement::default(),
			SecurityRequirement::from([(OPENAPI_SECURITY_NAME.to_string(), Vec::new())]),
		]);
	}
}

impl OperationInput for AuthenticatedPlayer {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		AuthenticationExtractor::operation_input(ctx, operation);
	}
}

impl OperationInput for ClientKind {}

const MISSING_AUTHORIZATION_ERR: (StatusCode, &str) =
	(StatusCode::UNAUTHORIZED, "Authorization header was missing");
const INVALID_AUTHORIZATION_ERR: (StatusCode, &str) =
	(StatusCode::UNAUTHORIZED, "Authorization header was invalid");
const BLACKLISTED_ERR: (StatusCode, &str) =
	(StatusCode::FORBIDDEN, "Authenticated player is blacklisted");
/// Holds the dashboard's session token. `HttpOnly`, so page scripts never see
/// it, and `__Host-` so no sibling subdomain can plant one of its own.
pub const SESSION_COOKIE: &str = "__Host-plus_session";
const OUT_OF_SCOPE_ERR: (StatusCode, &str) =
	(StatusCode::FORBIDDEN, "This token is limited to Special Chat");

/// Claim naming what a token was minted for, when it is not a plain player login.
const ACCESS_CLAIM: &str = "access";

/// What a token was minted for. Permission-gated endpoints only take
/// `Dashboard` tokens, so a token the game or a launcher holds (or that a mod
/// mints with the player's Minecraft session) never carries their roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAccess {
	/// `/login`: the game, launchers, scripts.
	Player,
	/// The admin dashboard's session.
	Dashboard,
	/// Staff answering Special Chat as a target account: only the requests
	/// [`special_chat_allows`] lists, and a receive-only websocket.
	SpecialChat,
}

impl TokenAccess {
	fn claim(self) -> Option<&'static str> {
		match self {
			Self::Player => None,
			Self::Dashboard => Some("dashboard"),
			Self::SpecialChat => Some("special-chat"),
		}
	}

	/// Unknown values fall to the most restricted kind.
	fn from_claims(claims: &Claims) -> Self {
		match claims.get_claim(ACCESS_CLAIM).map(serde_json::Value::as_str) {
			None => Self::Player,
			Some(Some("dashboard")) => Self::Dashboard,
			Some(_) => Self::SpecialChat,
		}
	}
}

impl OperationInput for TokenAccess {}

/// Exactly what the dashboard's Special Chat page sends as the target.
fn special_chat_allows(method: &http::Method, path: &str) -> bool {
	let segments: Vec<&str> = path.trim_end_matches('/').split('/').skip(1).collect();

	matches!(
		(method.as_str(), segments.as_slice()),
		("GET", ["groups"])
			| ("GET" | "POST", ["groups", _, "messages"])
			| ("POST", ["groups", _, "read", _])
			| ("POST", ["players", "resolve"])
			| ("GET", ["websocket"])
	)
}

impl ClientKind {
	/// Claim `/login` stamps when the caller reported mod and platform details,
	/// which only the in-game mod does.
	pub const CLAIM: &'static str = "game";

	fn from_claim(claim: Option<&serde_json::Value>) -> Self {
		match claim.and_then(serde_json::Value::as_bool) {
			Some(true) => Self::Game,
			_ => Self::Other,
		}
	}
}

/// The bearer token, or failing that the dashboard's session cookie.
fn request_token<'a>(
	parts: &'a Parts,
	state: &ApiState,
) -> Result<&'a str, (StatusCode, &'static str)> {
	if let Some(header) = parts.headers.get(AUTHORIZATION) {
		return header
			.to_str()
			.ok()
			.and_then(|header| header.strip_prefix("Bearer "))
			.ok_or(INVALID_AUTHORIZATION_ERR);
	}

	if let Some(token) = websocket_bearer(parts) {
		return Ok(token);
	}

	let token = session_cookie(parts).ok_or(MISSING_AUTHORIZATION_ERR)?;

	// Browsers attach the cookie to any same-site request, so only honour it
	// from the dashboard itself, not other polyfrost.org pages.
	if !from_dashboard(parts, state) {
		return Err(INVALID_AUTHORIZATION_ERR);
	}

	Ok(token)
}

/// Browsers cannot set headers on a websocket, so they offer the token as
/// subprotocols instead: `Sec-WebSocket-Protocol: bearer, <token>`.
fn websocket_bearer(parts: &Parts) -> Option<&str> {
	let mut protocols = parts
		.headers
		.get(http::header::SEC_WEBSOCKET_PROTOCOL)?
		.to_str()
		.ok()?
		.split(',')
		.map(str::trim);

	(protocols.next()? == "bearer").then(|| protocols.next()).flatten()
}

/// Whether the request carries any player credential, valid or not.
pub(crate) fn has_player_credential(parts: &Parts) -> bool {
	parts.headers.contains_key(AUTHORIZATION)
		|| websocket_bearer(parts).is_some()
		|| session_cookie(parts).is_some()
}

pub(crate) fn from_dashboard(parts: &Parts, state: &ApiState) -> bool {
	parts
		.headers
		.get(http::header::ORIGIN)
		.is_some_and(|origin| state.dashboard_origins.contains(origin))
}

fn session_cookie(parts: &Parts) -> Option<&str> {
	parts
		.headers
		.get_all(http::header::COOKIE)
		.iter()
		.filter_map(|header| header.to_str().ok())
		.flat_map(|header| header.split(';'))
		.find_map(|pair| {
			pair.trim()
				.strip_prefix(SESSION_COOKIE)?
				.strip_prefix('=')
		})
}

/// Mints a PASETO session token for `uuid`, valid for `ttl`.
pub(crate) fn mint_token(
	state: &ApiState,
	uuid: Uuid,
	is_game_client: bool,
	ttl: std::time::Duration,
	access: TokenAccess,
) -> Result<String, pasetors::errors::Error> {
	let mut claims = Claims::new()?;
	claims.set_expires_in(&ttl)?;
	claims.subject(&uuid.as_hyphenated().to_string())?;
	claims.add_additional(ClientKind::CLAIM, is_game_client)?;
	if let Some(access) = access.claim() {
		claims.add_additional(ACCESS_CLAIM, access)?;
	}

	local::encrypt(&state.paseto_key, &claims, None, PASETO_IMPLICIT_ASSERT)
}

/// Decrypts and validates the request's token, handing back its claims.
fn token_claims(
	parts: &Parts,
	state: &ApiState,
) -> Result<Claims, (StatusCode, &'static str)> {
	let token = request_token(parts, state)?;

	local::decrypt(
		&state.paseto_key,
		&UntrustedToken::<Local, V4>::try_from(token)
			.map_err(|_| INVALID_AUTHORIZATION_ERR)?,
		&ClaimsValidationRules::new(),
		None,
		PASETO_IMPLICIT_ASSERT,
	)
	.map_err(|_| INVALID_AUTHORIZATION_ERR)?
	.payload_claims()
	.cloned()
	.ok_or(MISSING_AUTHORIZATION_ERR)
}

impl FromRequestParts<ApiState> for AuthenticationExtractor {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		let claims = token_claims(parts, state).map_err(IntoResponse::into_response)?;

		if TokenAccess::from_claims(&claims) == TokenAccess::SpecialChat {
			// Nested routers strip their prefix from `uri`; check the full path.
			let path = parts
				.extensions
				.get::<axum::extract::OriginalUri>()
				.map_or(parts.uri.path(), |original| original.0.path());
			if !special_chat_allows(&parts.method, path) {
				return Err(OUT_OF_SCOPE_ERR.into_response());
			}
		}

		let sub = claims
			.get_claim("sub")
			.ok_or(MISSING_AUTHORIZATION_ERR.into_response())?
			.as_str()
			.ok_or(MISSING_AUTHORIZATION_ERR.into_response())?;

		Uuid::parse_str(sub)
			.map(Self)
			.map_err(|_| MISSING_AUTHORIZATION_ERR.into_response())
	}
}

impl FromRequestParts<ApiState> for ClientKind {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		Ok(Self::from_claim(
			token_claims(parts, state)
				.map_err(IntoResponse::into_response)?
				.get_claim(Self::CLAIM),
		))
	}
}

impl FromRequestParts<ApiState> for TokenAccess {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		Ok(Self::from_claims(
			&token_claims(parts, state).map_err(IntoResponse::into_response)?,
		))
	}
}

impl FromRequestParts<ApiState> for OptionalAuthenticationExtractor {
	type Rejection = <AuthenticationExtractor as FromRequestParts<ApiState>>::Rejection;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		// A stray cookie from a non-dashboard page is not a credential, so it
		// leaves the caller anonymous instead of failing the request.
		let has_credential = parts.headers.contains_key(AUTHORIZATION)
			|| websocket_bearer(parts).is_some()
			|| (session_cookie(parts).is_some() && from_dashboard(parts, state));
		Ok(Self(if has_credential {
			Some(
				AuthenticationExtractor::from_request_parts(parts, state)
					.await?
					.0,
			)
		} else {
			None
		}))
	}
}

impl FromRequestParts<ApiState> for AuthenticatedPlayer {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		let uuid = AuthenticationExtractor::from_request_parts(parts, state)
			.await?
			.0;
		let player = User::get_or_create(&state.database, uuid)
			.await
			.map_err(|e| {
				(
					StatusCode::INTERNAL_SERVER_ERROR,
					format!("Unable to load authenticated player: {e}"),
				)
					.into_response()
			})?;

		if player.blacklisted {
			return Err(BLACKLISTED_ERR.into_response());
		}

		Ok(Self(player))
	}
}

#[cfg(test)]
mod tests {
	use axum::http::Request;

	use super::{ClientKind, TokenAccess, special_chat_allows, websocket_bearer};

	#[test]
	fn special_chat_tokens_only_reach_what_the_chat_page_sends() {
		use http::Method;

		for (method, path) in [
			(Method::GET, "/groups"),
			(Method::GET, "/groups/"),
			(Method::GET, "/groups/7/messages"),
			(Method::POST, "/groups/7/messages"),
			(Method::POST, "/groups/7/read/12"),
			(Method::POST, "/players/resolve"),
			(Method::GET, "/websocket"),
		] {
			assert!(special_chat_allows(&method, path), "{method} {path}");
		}
		for (method, path) in [
			(Method::POST, "/groups"),
			(Method::POST, "/groups/dm/abc"),
			(Method::POST, "/groups/7/claim"),
			(Method::POST, "/groups/7/members/abc"),
			(Method::DELETE, "/groups/7/messages/3"),
			(Method::PUT, "/groups/7/messages/3"),
			(Method::DELETE, "/groups/7/messages"),
			(Method::GET, "/groupsx"),
			(Method::GET, "/v1/roles"),
			(Method::GET, "/v1/me"),
			(Method::POST, "/oidc/authorize"),
			(Method::POST, "/account/links/discord"),
			(Method::GET, "/"),
		] {
			assert!(!special_chat_allows(&method, path), "{method} {path}");
		}
	}

	#[test]
	fn token_access_reads_its_claim_and_fails_closed() {
		let with = |value: Option<serde_json::Value>| {
			let mut claims = pasetors::claims::Claims::new().unwrap();
			if let Some(value) = value {
				claims.add_additional(super::ACCESS_CLAIM, value).unwrap();
			}
			TokenAccess::from_claims(&claims)
		};

		assert_eq!(with(None), TokenAccess::Player);
		assert_eq!(with(Some("dashboard".into())), TokenAccess::Dashboard);
		assert_eq!(with(Some("special-chat".into())), TokenAccess::SpecialChat);
		assert_eq!(with(Some("admin".into())), TokenAccess::SpecialChat);
		assert_eq!(with(Some(true.into())), TokenAccess::SpecialChat);
	}

	#[test]
	fn websocket_bearer_reads_the_second_subprotocol() {
		let parts = |value: &str| {
			Request::builder()
				.header("sec-websocket-protocol", value)
				.body(())
				.unwrap()
				.into_parts()
				.0
		};

		assert_eq!(websocket_bearer(&parts("bearer, v4.local.x")), Some("v4.local.x"));
		assert_eq!(websocket_bearer(&parts("bearer,v4.local.x")), Some("v4.local.x"));
		assert_eq!(websocket_bearer(&parts("bearer")), None);
		assert_eq!(websocket_bearer(&parts("chat, v4.local.x")), None);
	}

	#[test]
	fn client_kind_defaults_to_other_without_a_truthy_claim() {
		assert_eq!(
			ClientKind::from_claim(Some(&serde_json::Value::Bool(true))),
			ClientKind::Game
		);
		assert_eq!(
			ClientKind::from_claim(Some(&serde_json::Value::Bool(false))),
			ClientKind::Other
		);
		// Tokens minted before the claim existed, and junk values.
		assert_eq!(ClientKind::from_claim(None), ClientKind::Other);
		assert_eq!(
			ClientKind::from_claim(Some(&serde_json::Value::String("yes".into()))),
			ClientKind::Other
		);
	}
}
