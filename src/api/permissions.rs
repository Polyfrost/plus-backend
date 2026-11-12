//! Role based access: every role carries u64 permission flags, and a player
//! holds the union of their roles' flags.

use aide::OperationInput;
use axum::{
	extract::FromRequestParts,
	http::{StatusCode, request::Parts},
	response::{IntoResponse, Response},
};
use entities::{prelude::*, role, user, user_role};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder};

use crate::api::{
	ApiState,
	api_tokens::{OPENAPI_SECURITY_NAME, TOKEN_HEADER},
	rate_limit::{ClientAddress, address_key},
	v0::account::{AuthenticatedPlayer, TokenAccess, has_player_credential},
};

/// Permission flags. Bit positions are stored in the database, so never
/// renumber one; retire it instead. Mirrored by the admin dashboard.
pub(crate) mod perm {
	/// Grants every other permission and bypasses the role hierarchy.
	pub(crate) const ADMINISTRATOR: u64 = 1 << 0;
	pub(crate) const MANAGE_ROLES: u64 = 1 << 1;
	pub(crate) const VIEW_ANALYTICS: u64 = 1 << 2;
	/// Cosmetics, assets, bundles, collections and tags.
	pub(crate) const MANAGE_COSMETICS: u64 = 1 << 3;
	/// Product listings and settings.
	pub(crate) const MANAGE_STORE: u64 = 1 << 4;
	pub(crate) const GRANT_COSMETICS: u64 = 1 << 5;
	pub(crate) const MANAGE_LINKS: u64 = 1 << 6;
	/// Looking up players and reading other players' transactions.
	pub(crate) const VIEW_PLAYERS: u64 = 1 << 7;
	pub(crate) const MANAGE_API_TOKENS: u64 = 1 << 8;
	pub(crate) const USE_SPECIAL_CHAT: u64 = 1 << 9;
	/// Revenue, refunds and chargebacks: the Monetization analytics, and the
	/// money figures inside other analytics and creator-code stats.
	pub(crate) const VIEW_REVENUE: u64 = 1 << 10;
	/// The order list, with its buyers and amounts.
	pub(crate) const VIEW_ORDERS: u64 = 1 << 11;
	/// Sales and coupons.
	pub(crate) const MANAGE_DISCOUNTS: u64 = 1 << 12;
	/// Creator codes.
	pub(crate) const MANAGE_REFERRALS: u64 = 1 << 13;

	/// Everything that works from the catalogue, which may therefore read it.
	pub(crate) const CATALOG_READERS: u64 = MANAGE_STORE
		| MANAGE_COSMETICS
		| GRANT_COSMETICS
		| MANAGE_DISCOUNTS
		| MANAGE_REFERRALS;
}

/// Whether `granted` covers every flag in `required`.
pub(crate) fn has(granted: u64, required: u64) -> bool {
	granted & perm::ADMINISTRATOR != 0 || granted & required == required
}

/// The stored bits, read back as the flags they were written from.
pub(crate) fn flags(role: &role::Model) -> u64 {
	role.permissions as u64
}

pub(crate) fn union(roles: &[role::Model]) -> u64 {
	roles.iter().fold(0, |acc, role| acc | flags(role))
}

/// A player's roles, highest position first.
pub(crate) async fn roles_of(
	db: &impl ConnectionTrait,
	user_id: i32,
) -> Result<Vec<role::Model>, DbErr> {
	Role::find()
		.inner_join(UserRole)
		.filter(user_role::Column::UserId.eq(user_id))
		.order_by_desc(role::Column::Position)
		.order_by_asc(role::Column::Id)
		.all(db)
		.await
}

/// Whether someone holding `actor` may create, edit, delete or hand out a
/// role at `position` with `permissions`: like Discord, only roles below your
/// highest one, and never with a flag you lack yourself.
pub(crate) fn may_manage(actor: &[role::Model], position: i32, permissions: u64) -> bool {
	let granted = union(actor);
	if granted & perm::ADMINISTRATOR != 0 {
		return true;
	}
	position < top(actor) && permissions & !granted == 0
}

fn top(roles: &[role::Model]) -> i32 {
	roles.iter().map(|role| role.position).max().unwrap_or(i32::MIN)
}

/// Whether `actor` may change another player's roles at all: like Discord,
/// only players ranked below them, and never an administrator.
pub(crate) fn may_manage_member(actor: &[role::Model], target: &[role::Model]) -> bool {
	if union(actor) & perm::ADMINISTRATOR != 0 {
		return true;
	}
	union(target) & perm::ADMINISTRATOR == 0 && top(target) < top(actor)
}

/// Flags an API token may never hold: each would let a service grant itself
/// more, and role management is for people in the hierarchy.
pub(crate) const KEY_FORBIDDEN: u64 =
	perm::ADMINISTRATOR | perm::MANAGE_ROLES | perm::MANAGE_API_TOKENS;

/// Who passed a [`Require`] gate.
#[derive(Debug)]
pub enum Caller {
	/// A player signed in to the dashboard.
	Player(user::Model),
	/// An external service, by its API token.
	Key { id: i32, label: String },
}

impl std::fmt::Display for Caller {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Player(player) => write!(f, "player {}", player.minecraft_uuid),
			Self::Key { id, label } => write!(f, "api token {id} ({label})"),
		}
	}
}

fn forbidden(message: &'static str) -> Response {
	(StatusCode::FORBIDDEN, message).into_response()
}

/// A dashboard player, or a service's API token, holding every flag in `P`,
/// with all the flags they hold for handlers that show more to some callers.
#[derive(Debug)]
pub struct Require<const P: u64>(pub Caller, pub u64);

impl<const P: u64> OperationInput for Require<P> {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		AuthenticatedPlayer::operation_input(ctx, operation);
		if P & KEY_FORBIDDEN == 0 {
			operation.security.push(aide::openapi::SecurityRequirement::from([(
				OPENAPI_SECURITY_NAME.to_string(),
				Vec::new(),
			)]));
		}
		// Documents the gate, and lets scripts/test-permissions.py find every guarded route.
		operation
			.extensions
			.insert("x-required-permission".into(), P.to_string().into());
	}
}

impl<const P: u64> FromRequestParts<ApiState> for Require<P> {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		let (caller, granted) = authenticate(parts, state, P).await?;
		if !has(granted, P) {
			return Err(denied(&caller));
		}
		Ok(Self(caller, granted))
	}
}

/// A dashboard player, or a service's API token, holding any flag in `P`: for
/// reads several otherwise unrelated permissions depend on.
#[derive(Debug)]
pub struct RequireAny<const P: u64>;

impl<const P: u64> OperationInput for RequireAny<P> {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		Require::<P>::operation_input(ctx, operation);
		operation.extensions.shift_remove("x-required-permission");
		operation
			.extensions
			.insert("x-required-any-permission".into(), P.to_string().into());
	}
}

impl<const P: u64> FromRequestParts<ApiState> for RequireAny<P> {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		let (caller, granted) = authenticate(parts, state, P).await?;
		if !has(granted, perm::ADMINISTRATOR) && granted & P == 0 {
			return Err(denied(&caller));
		}
		Ok(Self)
	}
}

fn denied(caller: &Caller) -> Response {
	forbidden(match caller {
		Caller::Player(_) => "Authenticated player does not have permission",
		Caller::Key { .. } => "API token does not have permission",
	})
}

/// Who is calling and every flag they hold. `required` only decides whether
/// an API token may be used at all.
async fn authenticate(
	parts: &mut Parts,
	state: &ApiState,
	required: u64,
) -> Result<(Caller, u64), Response> {
	// A key only counts when no player credential is sent, so a service
	// that forwards a player's token alongside its own acts as the player.
	if !has_player_credential(parts)
		&& let Some(key) = parts.headers.get(TOKEN_HEADER)
	{
		let key = key.to_str().unwrap_or_default().to_owned();
		return key_caller(parts, state, &key, required).await;
	}

	let player = AuthenticatedPlayer::from_request_parts(parts, state)
		.await?
		.0;

	// Only the dashboard's own sessions carry roles: a game or launcher
	// token, or one a mod minted from the player's Minecraft session, does not.
	if TokenAccess::from_request_parts(parts, state).await? != TokenAccess::Dashboard {
		return Err(forbidden("Permissions only apply to dashboard sessions"));
	}

	let roles = roles_of(&state.database, player.id).await.map_err(|error| {
		crate::api::error_response(StatusCode::INTERNAL_SERVER_ERROR, error)
	})?;

	Ok((Caller::Player(player), union(&roles)))
}

async fn key_caller(
	parts: &mut Parts,
	state: &ApiState,
	key: &str,
	required: u64,
) -> Result<(Caller, u64), Response> {
	let Ok(ClientAddress(ip)) = ClientAddress::from_request_parts(parts, state).await;
	let ip = ip.unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));

	let Some(resolved) = state.api_tokens.resolve(key, address_key(ip)).await else {
		return Err((StatusCode::UNAUTHORIZED, "Invalid or revoked API token").into_response());
	};
	if required & KEY_FORBIDDEN != 0 {
		return Err(forbidden("API token does not have permission"));
	}

	// Masked even though creation refuses them, so a hand-edited row cannot
	// smuggle in ADMINISTRATOR's bypass.
	Ok((
		Caller::Key {
			id: resolved.id,
			label: resolved.label.clone(),
		},
		resolved.permissions & !KEY_FORBIDDEN,
	))
}

/// [`Require`] for endpoints that act on the caller's place in the role
/// hierarchy, which only players have.
#[derive(Debug)]
pub struct RequirePlayer<const P: u64>(pub user::Model);

impl<const P: u64> OperationInput for RequirePlayer<P> {
	fn operation_input(
		ctx: &mut aide::generate::GenContext,
		operation: &mut aide::openapi::Operation,
	) {
		Require::<P>::operation_input(ctx, operation);
	}
}

impl<const P: u64> FromRequestParts<ApiState> for RequirePlayer<P> {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		match Require::<P>::from_request_parts(parts, state).await?.0 {
			Caller::Player(player) => Ok(Self(player)),
			Caller::Key { .. } => Err(forbidden("Only players can do this")),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{has, may_manage, may_manage_member, perm};

	fn role(position: i32, permissions: u64) -> entities::role::Model {
		entities::role::Model {
			id: position,
			name: format!("r{position}"),
			color: None,
			position,
			permissions: permissions as i64,
			created_at: chrono::Utc::now().fixed_offset(),
		}
	}

	#[test]
	fn flags_combine_and_administrator_implies_all() {
		let both = perm::MANAGE_STORE | perm::VIEW_ANALYTICS;
		assert!(has(both, perm::MANAGE_STORE));
		assert!(has(both, both));
		assert!(!has(perm::MANAGE_STORE, both));
		assert!(!has(0, perm::USE_SPECIAL_CHAT));
		assert!(has(perm::ADMINISTRATOR, perm::USE_SPECIAL_CHAT | 1 << 63));
	}

	#[test]
	fn bit_positions_match_the_split_migration() {
		// m20261113_000000_split_permissions hard-codes these.
		assert_eq!(perm::VIEW_ANALYTICS, 4);
		assert_eq!(perm::MANAGE_STORE, 16);
		assert_eq!(perm::VIEW_REVENUE, 1024);
		assert_eq!(perm::VIEW_ORDERS | perm::MANAGE_DISCOUNTS | perm::MANAGE_REFERRALS, 14336);
		assert!(!has(perm::VIEW_ANALYTICS, perm::VIEW_ANALYTICS | perm::VIEW_REVENUE));
	}

	#[test]
	fn high_bit_survives_the_signed_column() {
		let stored = role(0, 1 << 63);
		assert_eq!(super::flags(&stored), 1 << 63);
	}

	#[test]
	fn managers_only_reach_below_their_top_role_without_new_flags() {
		let manager = [role(5, perm::MANAGE_ROLES | perm::MANAGE_STORE), role(1, 0)];

		assert!(may_manage(&manager, 4, perm::MANAGE_STORE));
		assert!(!may_manage(&manager, 5, 0), "same position");
		assert!(!may_manage(&manager, 6, 0), "above");
		assert!(!may_manage(&manager, 1, perm::ADMINISTRATOR), "flag they lack");
		assert!(!may_manage(&[], 0, 0), "no roles at all");
		assert!(may_manage(&[role(0, perm::ADMINISTRATOR)], 99, u64::MAX));
	}

	#[test]
	fn managers_only_touch_members_ranked_below_them() {
		let manager = [role(5, perm::MANAGE_ROLES)];

		assert!(may_manage_member(&manager, &[]), "a player with no roles");
		assert!(may_manage_member(&manager, &[role(4, 0)]));
		assert!(!may_manage_member(&manager, &[role(5, 0)]), "a peer");
		assert!(!may_manage_member(&manager, &[role(9, 0)]), "a superior");
		assert!(!may_manage_member(&manager, &[role(0, perm::ADMINISTRATOR)]), "an admin");
		assert!(!may_manage_member(&[], &[]), "no roles at all");
		assert!(may_manage_member(&[role(0, perm::ADMINISTRATOR)], &[role(9, perm::ADMINISTRATOR)]));
	}
}
