use aide::{
	axum::{
		ApiRouter,
		routing::{delete_with, get_with, post_with, put_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::{IntoResponse, Response},
};
use entities::{
	parties,
	prelude::*,
	sea_orm_active_enums::{PartyInvitePolicy, PartyVoiceMode},
};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use uuid::Uuid;

use super::{
	PartyError,
	models::{Party, PartyLeftReason},
	roster::{
		broadcast, create_party, current_party, disband, find_member, party_view,
		remove_member, require_leader, require_party, room_name, touch,
	},
};
use crate::api::{ApiState, eos::spawn_with_retries, v0::account::AuthenticatedPlayer};

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct SettingsBody {
	#[serde(default)]
	voice_mode: Option<PartyVoiceMode>,
	#[serde(default)]
	invite_policy: Option<PartyInvitePolicy>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LeaderBody {
	player: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HardMuteBody {
	hard_muted: bool,
}

fn get_doc(op: TransformOperation) -> TransformOperation {
	op.id("getParty")
		.summary("Get your current party")
		.description(
			"Returns the caller's party with every member's live state, or 204 when \
			 the caller is not in a party.",
		)
		.tag("party")
		.response_with::<200, Json<Party>, _>(|res| res.description("The caller's party"))
		.response_with::<204, (), _>(|res| {
			res.description("The caller is not in a party")
		})
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createParty")
		.summary("Create a party")
		.description("Creates a party with the caller as its leader and only member.")
		.tag("party")
}

fn settings_doc(op: TransformOperation) -> TransformOperation {
	op.id("updatePartySettings")
		.summary("Change your party's settings")
		.description("Leader only. Fields left out keep their current value.")
		.tag("party")
}

fn leave_doc(op: TransformOperation) -> TransformOperation {
	op.id("leaveParty")
		.summary("Leave your party")
		.description(
			"Leadership passes to the longest-standing online member, or the \
			 longest-standing member when nobody is online. The party is deleted \
			 when its last member leaves.",
		)
		.tag("party")
}

fn disband_doc(op: TransformOperation) -> TransformOperation {
	op.id("disbandParty")
		.summary("Disband your party")
		.description("Leader only. Removes every member and deletes the party.")
		.tag("party")
}

fn leader_doc(op: TransformOperation) -> TransformOperation {
	op.id("transferPartyLeader")
		.summary("Hand party leadership to another member")
		.tag("party")
}

fn kick_doc(op: TransformOperation) -> TransformOperation {
	op.id("kickPartyMember")
		.summary("Kick a member from your party")
		.description("Leader only.")
		.tag("party")
}

fn hard_mute_doc(op: TransformOperation) -> TransformOperation {
	op.id("hardMutePartyMember")
		.summary("Hard-mute or unmute a member's voice")
		.description(
			"Leader only. Applied in the EOS voice room, and enforced by clients on \
			 peer-to-peer voice through `voice_hard_muted`.",
		)
		.tag("party")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/party",
			get_with(self::get, self::get_doc)
				.post_with(self::create, self::create_doc)
				.delete_with(self::disband_party, self::disband_doc),
		)
		.api_route(
			"/party/settings",
			put_with(self::settings, self::settings_doc),
		)
		.api_route("/party/leave", post_with(self::leave, self::leave_doc))
		.api_route("/party/leader", post_with(self::transfer, self::leader_doc))
		.api_route(
			"/party/members/{player}",
			delete_with(self::kick, self::kick_doc),
		)
		.api_route(
			"/party/members/{player}/voice",
			put_with(self::hard_mute, self::hard_mute_doc),
		)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn get(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Response, PartyError> {
	match current_party(&state, player.id).await? {
		Some((party, _)) => Ok(Json(party_view(&state, &party).await?).into_response()),
		None => Ok(StatusCode::NO_CONTENT.into_response()),
	}
}

#[tracing::instrument(level = "debug", skip(state))]
async fn create(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	body: Option<Json<SettingsBody>>,
) -> Result<(StatusCode, Json<Party>), PartyError> {
	let Json(body) = body.unwrap_or_default();
	let _guard = state.party.lock().await;

	if current_party(&state, player.id).await?.is_some() {
		return Err(PartyError::AlreadyInParty);
	}

	let party = create_party(
		&state,
		&player,
		body.voice_mode.unwrap_or(PartyVoiceMode::Party),
		body.invite_policy.unwrap_or(PartyInvitePolicy::Leader),
	)
	.await?;

	let view = broadcast(&state, party.id)
		.await?
		.ok_or(PartyError::NotInParty)?;
	Ok((StatusCode::CREATED, Json(view)))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn settings(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<SettingsBody>,
) -> Result<Json<Party>, PartyError> {
	let _guard = state.party.lock().await;

	let (party, _) = require_party(&state, player.id).await?;
	require_leader(&party, &player)?;

	let id = party.id;
	let mut active: parties::ActiveModel = party.into();
	if let Some(voice_mode) = body.voice_mode {
		active.voice_mode = Set(voice_mode);
	}
	if let Some(invite_policy) = body.invite_policy {
		active.invite_policy = Set(invite_policy);
	}
	if active.is_changed() {
		active.update(&state.database).await?;
		touch(&state.database, id).await?;
	}

	let view = broadcast(&state, id).await?.ok_or(PartyError::NotInParty)?;
	Ok(Json(view))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn leave(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let (party, _) = require_party(&state, player.id).await?;
	remove_member(&state, &party, &player, PartyLeftReason::Left).await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn disband_party(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let (party, _) = require_party(&state, player.id).await?;
	require_leader(&party, &player)?;
	disband(&state, &party).await?;

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn transfer(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<LeaderBody>,
) -> Result<Json<Party>, PartyError> {
	let _guard = state.party.lock().await;

	let (party, _) = require_party(&state, player.id).await?;
	require_leader(&party, &player)?;
	let (membership, _) = find_member(&state, party.id, body.player).await?;

	if membership.user_id != party.leader_id {
		Parties::update_many()
			.col_expr(parties::Column::LeaderId, membership.user_id.into())
			.filter(parties::Column::Id.eq(party.id))
			.exec(&state.database)
			.await?;
		touch(&state.database, party.id).await?;
	}

	let view = broadcast(&state, party.id)
		.await?
		.ok_or(PartyError::NotInParty)?;
	Ok(Json(view))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn kick(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let (party, _) = require_party(&state, player.id).await?;
	require_leader(&party, &player)?;
	if target == player.minecraft_uuid {
		return Err(PartyError::CannotKickSelf);
	}
	let (_, user) = find_member(&state, party.id, target).await?;

	remove_member(&state, &party, &user, PartyLeftReason::Kicked).await?;
	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn hard_mute(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
	Json(body): Json<HardMuteBody>,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let (party, _) = require_party(&state, player.id).await?;
	require_leader(&party, &player)?;
	let (membership, user) = find_member(&state, party.id, target).await?;

	if membership.voice_hard_muted != body.hard_muted {
		let mut active: entities::party_members::ActiveModel = membership.into();
		active.voice_hard_muted = Set(body.hard_muted);
		active.update(&state.database).await?;
		touch(&state.database, party.id).await?;
		broadcast(&state, party.id).await?;
	}

	if let (Some(eos), Some(puid)) = (state.eos.clone(), user.eos_product_user_id) {
		let http = state.client.clone();
		let room = room_name(party.id);
		let hard_muted = body.hard_muted;
		spawn_with_retries("hard mute", move || {
			let (eos, http, room, puid) =
				(eos.clone(), http.clone(), room.clone(), puid.clone());
			async move { eos.set_hard_muted(&http, &room, &puid, hard_muted).await }
		});
	}

	Ok(StatusCode::NO_CONTENT)
}
