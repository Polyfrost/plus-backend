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
	parties, party_members,
	prelude::*,
	sea_orm_active_enums::{PartyInvitePolicy, PartyVoiceMode},
	user,
};
use schemars::JsonSchema;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use uuid::Uuid;

use super::{
	PartyError,
	models::{Party, PartyLeftReason},
	roster::{
		Member, broadcast, create_party, current_party, disband, find_member,
		party_view, remove_member, require_leader, require_party, room_name, touch,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct SoundboardMuteBody {
	muted: bool,
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

fn soundboard_mute_doc(op: TransformOperation) -> TransformOperation {
	op.id("soundboardMutePartyMember")
		.summary("Mute or unmute a member's soundboard")
		.description(
			"Leader only. Clients stop sending and playing that member's soundboard \
			 sounds while `soundboard_muted` is set. Their voice is not affected.",
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
		.api_route(
			"/party/members/{player}/soundboard",
			put_with(self::soundboard_mute, self::soundboard_mute_doc),
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

async fn led_member(
	state: &ApiState,
	player: &user::Model,
	target: Uuid,
) -> Result<(parties::Model, Member), PartyError> {
	let (party, _) = require_party(state, player.id).await?;
	require_leader(&party, player)?;
	let member = find_member(state, party.id, target).await?;
	Ok((party, member))
}

fn hard_mute_change(
	membership: party_members::Model,
	hard_muted: bool,
) -> Option<party_members::ActiveModel> {
	(membership.voice_hard_muted != hard_muted).then(|| {
		let mut active: party_members::ActiveModel = membership.into();
		active.voice_hard_muted = Set(hard_muted);
		active
	})
}

fn soundboard_mute_change(
	membership: party_members::Model,
	muted: bool,
) -> Option<party_members::ActiveModel> {
	(membership.soundboard_muted != muted).then(|| {
		let mut active: party_members::ActiveModel = membership.into();
		active.soundboard_muted = Set(muted);
		active
	})
}

#[tracing::instrument(level = "debug", skip(state))]
async fn hard_mute(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
	Json(body): Json<HardMuteBody>,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let (party, (membership, user)) = led_member(&state, &player, target).await?;

	if let Some(active) = hard_mute_change(membership, body.hard_muted) {
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

#[tracing::instrument(level = "debug", skip(state))]
async fn soundboard_mute(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
	Json(body): Json<SoundboardMuteBody>,
) -> Result<StatusCode, PartyError> {
	let _guard = state.party.lock().await;

	let (party, (membership, _)) = led_member(&state, &player, target).await?;

	if let Some(active) = soundboard_mute_change(membership, body.muted) {
		active.update(&state.database).await?;
		touch(&state.database, party.id).await?;
		broadcast(&state, party.id).await?;
	}

	Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
	use chrono::Utc;
	use sea_orm::{ActiveValue::Unchanged, TryIntoModel};

	use super::*;
	use crate::api::v1::party::roster::member_view;

	fn player(id: i32) -> user::Model {
		user::Model {
			id,
			minecraft_uuid: Uuid::new_v4(),
			created_at: Utc::now().fixed_offset(),
			blacklisted: false,
			particle_color: None,
			refund_count: 0,
			eos_product_user_id: None,
			username: None,
			chargeback_count: 0,
			paynow_customer_id: None,
			active_outfit_id: None,
			outfits_seeded: false,
			status_override: None,
			eos_product_user_id_verified_at: None,
		}
	}

	fn membership() -> party_members::Model {
		party_members::Model {
			party_id: Uuid::nil(),
			user_id: 2,
			joined_at: Utc::now().fixed_offset(),
			voice_hard_muted: false,
			soundboard_muted: false,
		}
	}

	fn apply(change: Option<party_members::ActiveModel>) -> party_members::Model {
		change
			.expect("the mute should change the row")
			.try_into_model()
			.expect("every column should be set")
	}

	#[test]
	fn non_leaders_get_the_hard_mute_rejection() {
		let party = parties::Model {
			id: Uuid::nil(),
			leader_id: 1,
			voice_mode: PartyVoiceMode::Party,
			invite_policy: PartyInvitePolicy::Leader,
			chat_group_id: None,
			created_at: Utc::now().fixed_offset(),
			updated_at: Utc::now().fixed_offset(),
		};

		let rejection = require_leader(&party, &player(2))
			.expect_err("only the leader may change a member's mutes");
		assert!(matches!(rejection, PartyError::NotLeader));
		assert_eq!(rejection.code(), "not_leader");
		assert_eq!(rejection.into_response().status(), StatusCode::FORBIDDEN);
		assert!(require_leader(&party, &player(1)).is_ok());
	}

	#[test]
	fn muting_shows_in_the_party_view_and_unmuting_clears_it() {
		let muted = apply(soundboard_mute_change(membership(), true));
		assert!(member_view(muted.clone(), player(2), true, None).soundboard_muted);

		let unmuted = apply(soundboard_mute_change(muted, false));
		assert!(!member_view(unmuted, player(2), true, None).soundboard_muted);
	}

	#[test]
	fn setting_the_current_value_changes_nothing() {
		assert!(soundboard_mute_change(membership(), false).is_none());
		assert!(hard_mute_change(membership(), false).is_none());
	}

	#[test]
	fn soundboard_mute_leaves_the_hard_mute_alone() {
		let hard_muted = party_members::Model {
			voice_hard_muted: true,
			..membership()
		};
		let change = soundboard_mute_change(hard_muted, true)
			.expect("muting changes the row");
		assert_eq!(change.voice_hard_muted, Unchanged(true));

		let view = member_view(apply(Some(change)), player(2), true, None);
		assert!(view.voice_hard_muted);
		assert!(view.soundboard_muted);
	}

	#[test]
	fn hard_mute_leaves_the_soundboard_mute_alone() {
		let soundboard_muted = party_members::Model {
			soundboard_muted: true,
			..membership()
		};
		let change = hard_mute_change(soundboard_muted, true)
			.expect("hard-muting changes the row");
		assert_eq!(change.soundboard_muted, Unchanged(true));

		let view = member_view(apply(Some(change)), player(2), true, None);
		assert!(view.voice_hard_muted);
		assert!(view.soundboard_muted);
	}
}
