use std::net::IpAddr;

use aide::{
	OperationInput,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{FromRequestParts, State},
	http::request::Parts,
	response::Response,
};
use axum_client_ip::ClientIp;
use chrono::Utc;
use entities::{prelude::*, user};
use jsonwebtoken::{Algorithm, Header, encode};
use sea_orm::{ActiveModelTrait, DbErr, EntityTrait, Set};
use serde::Serialize;
use tracing::warn;

use super::{
	PartyError,
	models::{VoicePass, VoiceToken, VoiceTransport},
	roster::{party_view, require_party, room_name},
};
use crate::api::{
	ApiState,
	eos::{EosClient, EosError},
	v0::account::AuthenticatedPlayer,
};

const PASS_AUDIENCE: &str = "polyplus-party-voice";
const PASS_TTL: chrono::Duration = chrono::Duration::minutes(10);
const VERIFICATION_TTL: chrono::Duration = chrono::Duration::hours(24);

pub(super) struct CallerIp(Option<IpAddr>);

impl OperationInput for CallerIp {
	fn operation_input(
		_ctx: &mut aide::generate::GenContext,
		_operation: &mut aide::openapi::Operation,
	) {
	}
}

impl FromRequestParts<ApiState> for CallerIp {
	type Rejection = Response;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &ApiState,
	) -> Result<Self, Self::Rejection> {
		Ok(Self(
			ClientIp::from_request_parts(parts, state)
				.await
				.map(|ClientIp(ip)| ip)
				.ok(),
		))
	}
}

#[derive(Serialize)]
struct PassClaims {
	iss: String,
	aud: &'static str,
	sub: String,
	puid: String,
	party: String,
	iat: i64,
	exp: i64,
}

fn token_doc(op: TransformOperation) -> TransformOperation {
	op.id("createPartyVoiceToken")
		.summary("Get a token for your party's EOS voice room")
		.description(
			"Asks Epic Online Services for a participant token for the caller's party \
			 voice room. Only works while the party's voice transport is `rtc`. \
			 Returns 503 `voice_unavailable` when voice is not configured on the \
			 backend or Epic fails, and the client should fall back to peer-to-peer \
			 voice. Limited to 10 per minute.",
		)
		.tag("party")
}

fn pass_doc(op: TransformOperation) -> TransformOperation {
	op.id("createPartyVoicePass")
		.summary("Get a signed pass for peer-to-peer party voice")
		.description(
			"Returns an RS256 JWT, signed with the key published at \
			 `/.well-known/jwks.json`, that proves the caller's EOS ProductUserId \
			 belongs to a member of their party. It expires after 10 minutes. \
			 Limited to 10 per minute.",
		)
		.tag("party")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/party/voice/token",
			post_with(self::token, self::token_doc),
		)
		.api_route("/party/voice/pass", post_with(self::pass, self::pass_doc))
}

async fn lookup_and_store(
	state: &ApiState,
	eos: &EosClient,
	user: &user::Model,
) -> Result<Option<String>, LookupError> {
	let verified = eos
		.product_user_id(&state.client, &user.minecraft_uuid.to_string())
		.await?;

	let mut active: user::ActiveModel = user.clone().into();
	match &verified {
		Some(puid) => {
			if user
				.eos_product_user_id
				.as_ref()
				.is_some_and(|claimed| claimed != puid)
			{
				warn!(
					player = %user.minecraft_uuid,
					"Claimed EOS ProductUserId differs from Epic's, storing Epic's"
				);
			}
			active.eos_product_user_id = Set(Some(puid.clone()));
			active.eos_product_user_id_verified_at = Set(Some(Utc::now().into()));
		}
		None => active.eos_product_user_id_verified_at = Set(None),
	}
	if active.is_changed() {
		active.update(&state.database).await?;
	}

	Ok(verified)
}

#[derive(thiserror::Error, Debug)]
enum LookupError {
	#[error(transparent)]
	Eos(#[from] EosError),
	#[error(transparent)]
	Database(#[from] DbErr),
}

async fn verified_puid(
	state: &ApiState,
	user: &user::Model,
) -> Result<String, PartyError> {
	let previously_verified = user
		.eos_product_user_id_verified_at
		.and(user.eos_product_user_id.clone());
	let fresh = user
		.eos_product_user_id_verified_at
		.is_some_and(|at| Utc::now() - at.with_timezone(&Utc) < VERIFICATION_TTL);

	let Some(eos) = state.eos.as_ref().filter(|_| !fresh) else {
		return previously_verified.ok_or(PartyError::EosNotLinked);
	};

	match lookup_and_store(state, eos, user).await {
		Ok(Some(puid)) => Ok(puid),
		Ok(None) => Err(PartyError::EosNotLinked),
		Err(LookupError::Database(error)) => Err(error.into()),
		Err(LookupError::Eos(error)) => {
			warn!(player = %user.minecraft_uuid, "Unable to verify EOS ProductUserId: {error}");
			previously_verified.ok_or(PartyError::VoiceUnavailable)
		}
	}
}

pub(in crate::api) fn spawn_puid_verification(state: &ApiState, user_id: i32) {
	let Some(eos) = state.eos.clone() else {
		return;
	};
	let state = state.clone();

	tokio::spawn(async move {
		let user = match User::find_by_id(user_id).one(&state.database).await {
			Ok(Some(user)) => user,
			Ok(None) => return,
			Err(error) => {
				warn!("Unable to load player {user_id} to verify their PUID: {error}");
				return;
			}
		};

		match lookup_and_store(&state, &eos, &user).await {
			Ok(Some(_)) => {}
			Ok(None) => warn!(
				player = %user.minecraft_uuid,
				"Epic has no EOS ProductUserId for this player yet"
			),
			Err(error) => warn!(
				player = %user.minecraft_uuid,
				"Unable to verify EOS ProductUserId: {error}"
			),
		}
	});
}

#[tracing::instrument(level = "debug", skip(state))]
async fn token(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	CallerIp(client_ip): CallerIp,
) -> Result<Json<VoiceToken>, PartyError> {
	let Some(eos) = state.eos.clone() else {
		return Err(PartyError::VoiceUnavailable);
	};
	if let Some(wait) = state.voice_token_limit.check(player.id).await {
		return Err(PartyError::RateLimited(wait));
	}

	let (party, membership) = require_party(&state, player.id).await?;
	if party_view(&state, &party).await?.voice.transport != VoiceTransport::Rtc {
		return Err(PartyError::TransportP2p);
	}

	let puid = verified_puid(&state, &player).await?;
	let room = room_name(party.id);
	let issued = eos
		.create_room_token(
			&state.client,
			&room,
			&puid,
			client_ip,
			membership.voice_hard_muted,
		)
		.await
		.map_err(|error| {
			warn!(player = %player.minecraft_uuid, "Unable to create EOS room token: {error}");
			PartyError::VoiceUnavailable
		})?;

	Ok(Json(VoiceToken {
		room_name: room,
		client_base_url: issued.client_base_url,
		participant_token: issued.participant_token,
		issued_at: Utc::now(),
	}))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn pass(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<VoicePass>, PartyError> {
	if let Some(wait) = state.voice_pass_limit.check(player.id).await {
		return Err(PartyError::RateLimited(wait));
	}
	let (party, _) = require_party(&state, player.id).await?;

	let puid = verified_puid(&state, &player).await?;
	let issued_at = Utc::now();
	let expires_at = issued_at + PASS_TTL;

	let claims = PassClaims {
		iss: state.oidc_issuer.trim_end_matches('/').to_string(),
		aud: PASS_AUDIENCE,
		sub: player.minecraft_uuid.to_string(),
		puid,
		party: party.id.to_string(),
		iat: issued_at.timestamp(),
		exp: expires_at.timestamp(),
	};

	let mut header = Header::new(Algorithm::RS256);
	header.kid = Some(state.oidc_signing_key.kid.clone());
	let pass = encode(&header, &claims, &state.oidc_signing_key.encoding_key)?;

	Ok(Json(VoicePass { pass, expires_at }))
}
