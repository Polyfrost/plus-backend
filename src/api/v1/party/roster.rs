use std::{
	collections::{HashMap, HashSet},
	time::Duration,
};

use chrono::Utc;
use entities::{
	group_members, groups, parties, party_members,
	prelude::*,
	sea_orm_active_enums::{
		GroupKind, GroupMemberRole, PartyInvitePolicy, PartyVoiceMode,
	},
	user,
};
use sea_orm::{
	ActiveValue, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait,
	QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait, sea_query::OnConflict,
};
use tracing::warn;
use uuid::Uuid;

use super::{
	PartyError, invites, join,
	live::Forward,
	models::{
		MemberState, Party, PartyLeftReason, PartyMember, PartySettings, PartyVoice,
		PlayEndReason, VoiceTransport,
	},
	play,
};
use crate::api::{
	ApiState,
	eos::spawn_with_retries,
	v0::websocket::{send_to_owner, structs::ClientBoundPacket},
};

pub(super) const MAX_PARTY_SIZE: usize = 8;
const OFFLINE_GRACE: Duration = Duration::from_secs(5 * 60);
const STATE_FORWARD_INTERVAL: Duration = Duration::from_millis(250);

type Member = (party_members::Model, user::Model);

pub(super) fn room_name(party: Uuid) -> String {
	format!("p{}", party.simple())
}

pub(super) async fn find_user(
	state: &ApiState,
	player: Uuid,
) -> Result<Option<user::Model>, DbErr> {
	User::find()
		.filter(user::Column::MinecraftUuid.eq(player))
		.one(&state.database)
		.await
}

pub(super) async fn uuids_of(
	state: &ApiState,
	ids: impl IntoIterator<Item = i32>,
) -> Result<HashMap<i32, Uuid>, DbErr> {
	let ids = ids.into_iter().collect::<HashSet<_>>();
	if ids.is_empty() {
		return Ok(HashMap::new());
	}

	Ok(User::find()
		.filter(user::Column::Id.is_in(ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect())
}

pub(super) async fn current_party(
	state: &ApiState,
	user_id: i32,
) -> Result<Option<(parties::Model, party_members::Model)>, DbErr> {
	let Some(membership) = PartyMembers::find()
		.filter(party_members::Column::UserId.eq(user_id))
		.one(&state.database)
		.await?
	else {
		return Ok(None);
	};

	Ok(Parties::find_by_id(membership.party_id)
		.one(&state.database)
		.await?
		.map(|party| (party, membership)))
}

pub(super) async fn require_party(
	state: &ApiState,
	user_id: i32,
) -> Result<(parties::Model, party_members::Model), PartyError> {
	current_party(state, user_id)
		.await?
		.ok_or(PartyError::NotInParty)
}

pub(super) fn require_leader(
	party: &parties::Model,
	user: &user::Model,
) -> Result<(), PartyError> {
	if party.leader_id == user.id {
		Ok(())
	} else {
		Err(PartyError::NotLeader)
	}
}

pub(super) async fn member_count(state: &ApiState, party: Uuid) -> Result<usize, DbErr> {
	let count = PartyMembers::find()
		.filter(party_members::Column::PartyId.eq(party))
		.count(&state.database)
		.await?;
	Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

pub(super) async fn find_member(
	state: &ApiState,
	party: Uuid,
	player: Uuid,
) -> Result<Member, PartyError> {
	let user = find_user(state, player)
		.await?
		.ok_or(PartyError::MemberNotFound)?;
	let membership = PartyMembers::find_by_id((party, user.id))
		.one(&state.database)
		.await?
		.ok_or(PartyError::MemberNotFound)?;
	Ok((membership, user))
}

pub(super) async fn members_of(
	db: &impl ConnectionTrait,
	party: Uuid,
) -> Result<Vec<Member>, DbErr> {
	Ok(PartyMembers::find()
		.filter(party_members::Column::PartyId.eq(party))
		.order_by_asc(party_members::Column::JoinedAt)
		.find_also_related(User)
		.all(db)
		.await?
		.into_iter()
		.filter_map(|(membership, user)| Some((membership, user?)))
		.collect())
}

pub(super) async fn member_uuids(
	state: &ApiState,
	party: Uuid,
) -> Result<Vec<Uuid>, DbErr> {
	Ok(members_of(&state.database, party)
		.await?
		.into_iter()
		.map(|(_, user)| user.minecraft_uuid)
		.collect())
}

pub(super) async fn connected(state: &ApiState, player: Uuid) -> bool {
	state
		.realtime
		.connections_by_owner
		.read()
		.await
		.contains_key(&player)
}

fn transport_for<'a>(
	state: &ApiState,
	states: impl IntoIterator<Item = &'a MemberState>,
) -> VoiceTransport {
	if state.eos.is_some() && !states.into_iter().any(MemberState::blocks_rtc) {
		VoiceTransport::Rtc
	} else {
		VoiceTransport::P2p
	}
}

pub(super) async fn party_view(
	state: &ApiState,
	party: &parties::Model,
) -> Result<Party, DbErr> {
	let members = members_of(&state.database, party.id).await?;
	let players = members
		.iter()
		.map(|(_, user)| user.minecraft_uuid)
		.collect::<Vec<_>>();

	let online = {
		let connections = state.realtime.connections_by_owner.read().await;
		members
			.iter()
			.filter(|(_, user)| connections.contains_key(&user.minecraft_uuid))
			.map(|(_, user)| user.minecraft_uuid)
			.collect::<HashSet<_>>()
	};
	let mut states = state.party.states_of(&players).await;
	let transport = transport_for(state, states.values());

	let leader = match members.iter().find(|(_, user)| user.id == party.leader_id) {
		Some((_, user)) => user.minecraft_uuid,
		None => User::find_by_id(party.leader_id)
			.one(&state.database)
			.await?
			.map(|user| user.minecraft_uuid)
			.unwrap_or_default(),
	};

	state.party.sync_roster(party.id, players, transport).await;

	Ok(Party {
		id: party.id,
		leader,
		created_at: party.created_at.with_timezone(&Utc),
		settings: PartySettings {
			voice_mode: party.voice_mode,
			invite_policy: party.invite_policy,
		},
		chat_group_id: party.chat_group_id,
		voice: PartyVoice {
			room_name: room_name(party.id),
			transport,
			rtc_available: state.eos.is_some(),
		},
		members: members
			.into_iter()
			.map(|(membership, user)| {
				let online = online.contains(&user.minecraft_uuid);
				PartyMember {
					player: user.minecraft_uuid,
					joined_at: membership.joined_at.with_timezone(&Utc),
					online,
					eos_product_user_id: user
						.eos_product_user_id_verified_at
						.and(user.eos_product_user_id),
					voice_hard_muted: membership.voice_hard_muted,
					state: states.remove(&user.minecraft_uuid).filter(|_| online),
				}
			})
			.collect(),
	})
}

pub(super) async fn broadcast(
	state: &ApiState,
	party: Uuid,
) -> Result<Option<Party>, DbErr> {
	let Some(party) = Parties::find_by_id(party).one(&state.database).await? else {
		state.party.forget_party(party).await;
		return Ok(None);
	};

	let view = party_view(state, &party).await?;
	for member in &view.members {
		send_to_owner(state, member.player, || ClientBoundPacket::PartyUpdated {
			party: view.clone(),
		})
		.await;
	}

	Ok(Some(view))
}

pub(super) async fn broadcast_or_log(state: &ApiState, party: Uuid) {
	if let Err(error) = broadcast(state, party).await {
		warn!("Unable to push party {party} to its members: {error}");
	}
}

async fn insert_member(
	db: &impl ConnectionTrait,
	party: &parties::Model,
	user_id: i32,
) -> Result<(), DbErr> {
	PartyMembers::insert(party_members::ActiveModel {
		party_id: Set(party.id),
		user_id: Set(user_id),
		joined_at: ActiveValue::NotSet,
		voice_hard_muted: Set(false),
	})
	.exec_without_returning(db)
	.await?;

	if let Some(group_id) = party.chat_group_id {
		GroupMembers::insert(group_members::ActiveModel {
			group_id: Set(group_id),
			user_id: Set(user_id),
			role: Set(GroupMemberRole::Member),
			joined_at: ActiveValue::NotSet,
		})
		.on_conflict(
			OnConflict::columns([
				group_members::Column::GroupId,
				group_members::Column::UserId,
			])
			.do_nothing()
			.to_owned(),
		)
		.exec_without_returning(db)
		.await?;
	}

	Ok(())
}

pub(super) async fn create_party(
	state: &ApiState,
	leader: &user::Model,
	voice_mode: PartyVoiceMode,
	invite_policy: PartyInvitePolicy,
) -> Result<parties::Model, DbErr> {
	let txn = state.database.begin().await?;

	let group = Groups::insert(groups::ActiveModel {
		kind: Set(GroupKind::Party),
		name: Set(None),
		owner_id: Set(None),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.exec_with_returning(&txn)
	.await?;

	let party = Parties::insert(parties::ActiveModel {
		id: Set(Uuid::now_v7()),
		leader_id: Set(leader.id),
		voice_mode: Set(voice_mode),
		invite_policy: Set(invite_policy),
		chat_group_id: Set(Some(group.id)),
		created_at: ActiveValue::NotSet,
		updated_at: ActiveValue::NotSet,
	})
	.exec_with_returning(&txn)
	.await?;

	insert_member(&txn, &party, leader.id).await?;
	txn.commit().await?;

	Ok(party)
}

pub(super) async fn add_member(
	state: &ApiState,
	party: &parties::Model,
	user: &user::Model,
) -> Result<(), DbErr> {
	let txn = state.database.begin().await?;
	insert_member(&txn, party, user.id).await?;
	touch(&txn, party.id).await?;
	txn.commit().await
}

pub(super) async fn touch(db: &impl ConnectionTrait, party: Uuid) -> Result<(), DbErr> {
	Parties::update_many()
		.col_expr(
			parties::Column::UpdatedAt,
			sea_orm::sea_query::Expr::current_timestamp().into(),
		)
		.filter(parties::Column::Id.eq(party))
		.exec(db)
		.await?;
	Ok(())
}

async fn end_play_if_leader(
	state: &ApiState,
	party: &parties::Model,
	user: &user::Model,
) -> Result<(), DbErr> {
	if party.leader_id == user.id {
		let players = member_uuids(state, party.id).await?;
		play::end(state, party.id, PlayEndReason::LeaderLeft, &players).await;
	}
	Ok(())
}

/// Returns whether the party is now empty.
async fn detach_member(
	state: &ApiState,
	db: &impl ConnectionTrait,
	party: &parties::Model,
	user: &user::Model,
) -> Result<bool, DbErr> {
	PartyMembers::delete_by_id((party.id, user.id))
		.exec(db)
		.await?;
	if let Some(group_id) = party.chat_group_id {
		GroupMembers::delete_by_id((group_id, user.id))
			.exec(db)
			.await?;
	}

	let remaining = members_of(db, party.id).await?;
	if !remaining.is_empty() && party.leader_id == user.id {
		let online = {
			let connections = state.realtime.connections_by_owner.read().await;
			remaining
				.iter()
				.find(|(_, member)| connections.contains_key(&member.minecraft_uuid))
				.map(|(membership, _)| membership.user_id)
		};
		let next = online
			.or_else(|| remaining.first().map(|(membership, _)| membership.user_id));

		if let Some(next) = next {
			Parties::update_many()
				.col_expr(parties::Column::LeaderId, next.into())
				.filter(parties::Column::Id.eq(party.id))
				.exec(db)
				.await?;
		}
	}
	if !remaining.is_empty() {
		touch(db, party.id).await?;
	}

	Ok(remaining.is_empty())
}

async fn finish_removal(
	state: &ApiState,
	party: &parties::Model,
	user: &user::Model,
	reason: PartyLeftReason,
	empty: bool,
) -> Result<(), DbErr> {
	state.party.forget_seen(user.minecraft_uuid).await;
	join::cancel_hosted_joins_in_party(state, party.id, Some(user.minecraft_uuid)).await;
	leave_voice_room(state, party.id, user);
	send_to_owner(state, user.minecraft_uuid, || {
		ClientBoundPacket::PartyLeft {
			party_id: party.id,
			reason,
		}
	})
	.await;

	if empty {
		delete_party(state, party).await
	} else {
		invites::cancel_pending_from(state, party.id, user.id).await?;
		broadcast(state, party.id).await.map(|_| ())
	}
}

pub(super) async fn remove_member(
	state: &ApiState,
	party: &parties::Model,
	user: &user::Model,
	reason: PartyLeftReason,
) -> Result<(), DbErr> {
	end_play_if_leader(state, party, user).await?;

	let txn = state.database.begin().await?;
	let empty = detach_member(state, &txn, party, user).await?;
	txn.commit().await?;

	finish_removal(state, party, user, reason, empty).await
}

/// Moves `user` between parties in one transaction, so a failed join leaves
/// them where they were.
pub(super) async fn move_member(
	state: &ApiState,
	from: &parties::Model,
	to: &parties::Model,
	user: &user::Model,
) -> Result<(), DbErr> {
	end_play_if_leader(state, from, user).await?;

	let txn = state.database.begin().await?;
	let empty = detach_member(state, &txn, from, user).await?;
	insert_member(&txn, to, user.id).await?;
	touch(&txn, to.id).await?;
	txn.commit().await?;

	finish_removal(state, from, user, PartyLeftReason::Left, empty).await
}

pub(super) async fn disband(
	state: &ApiState,
	party: &parties::Model,
) -> Result<(), DbErr> {
	let members = members_of(&state.database, party.id).await?;
	let players = members
		.iter()
		.map(|(_, user)| user.minecraft_uuid)
		.collect::<Vec<_>>();
	play::end(state, party.id, PlayEndReason::LeaderLeft, &players).await;
	delete_party(state, party).await?;

	for (_, user) in members {
		state.party.forget_seen(user.minecraft_uuid).await;
		leave_voice_room(state, party.id, &user);
		send_to_owner(state, user.minecraft_uuid, || {
			ClientBoundPacket::PartyLeft {
				party_id: party.id,
				reason: PartyLeftReason::Disbanded,
			}
		})
		.await;
	}

	Ok(())
}

async fn delete_party(state: &ApiState, party: &parties::Model) -> Result<(), DbErr> {
	invites::cancel_pending_for_party(state, party.id).await?;
	join::cancel_hosted_joins_in_party(state, party.id, None).await;

	let txn = state.database.begin().await?;
	Parties::delete_by_id(party.id).exec(&txn).await?;
	if let Some(group_id) = party.chat_group_id {
		Groups::delete_by_id(group_id).exec(&txn).await?;
	}
	txn.commit().await?;

	state.party.forget_party(party.id).await;
	Ok(())
}

fn leave_voice_room(state: &ApiState, party: Uuid, user: &user::Model) {
	let (Some(eos), Some(puid)) = (state.eos.clone(), user.eos_product_user_id.clone())
	else {
		return;
	};
	let http = state.client.clone();
	let room = room_name(party);

	spawn_with_retries("participant removal", move || {
		let (eos, http, room, puid) =
			(eos.clone(), http.clone(), room.clone(), puid.clone());
		async move { eos.remove_participant(&http, &room, &puid).await }
	});
}

async fn lookup_party_id(
	state: &ApiState,
	player: Uuid,
	user_id: Option<i32>,
) -> Result<Option<Uuid>, DbErr> {
	if let Some(party) = state.party.party_of(player).await {
		return Ok(Some(party));
	}

	let query = PartyMembers::find();
	let membership = match user_id {
		Some(user_id) => {
			query
				.filter(party_members::Column::UserId.eq(user_id))
				.one(&state.database)
				.await?
		}
		None => {
			query
				.inner_join(User)
				.filter(user::Column::MinecraftUuid.eq(player))
				.one(&state.database)
				.await?
		}
	};
	let Some(membership) = membership else {
		return Ok(None);
	};

	if let Some(party) = Parties::find_by_id(membership.party_id)
		.one(&state.database)
		.await?
	{
		party_view(state, &party).await?;
	}

	Ok(Some(membership.party_id))
}

pub(in crate::api) async fn on_player_online(state: &ApiState, player: Uuid) {
	state.party.forget_seen(player).await;
	refresh_party(state, player).await;
}

pub(in crate::api) async fn refresh_party(state: &ApiState, player: Uuid) {
	match lookup_party_id(state, player, None).await {
		Ok(Some(party)) => broadcast_or_log(state, party).await,
		Ok(None) => {}
		Err(error) => warn!("Unable to look up the party of {player}: {error}"),
	}
}

pub(in crate::api) async fn on_player_offline(state: &ApiState, player: Uuid) {
	let party = match lookup_party_id(state, player, None).await {
		Ok(party) => party,
		Err(error) => {
			warn!("Unable to look up the party of {player}: {error}");
			None
		}
	};
	state.party.went_offline(player, party.is_some()).await;
	join::cancel_hosted_joins_of_player(state, player).await;

	let Some(party) = party else {
		return;
	};
	broadcast_or_log(state, party).await;

	let state = state.clone();
	tokio::spawn(async move {
		tokio::time::sleep(OFFLINE_GRACE).await;
		expire_if_offline(&state, player).await;
	});
}

async fn expire_if_offline(state: &ApiState, player: Uuid) {
	if connected(state, player).await
		|| state.party.offline_for(player).await < OFFLINE_GRACE
	{
		return;
	}

	let _guard = state.party.lock().await;
	if connected(state, player).await {
		return;
	}

	let removed = async {
		let Some(user) = find_user(state, player).await? else {
			return Ok(());
		};
		let Some((party, _)) = current_party(state, user.id).await? else {
			return Ok(());
		};
		remove_member(state, &party, &user, PartyLeftReason::Expired).await
	}
	.await;

	if let Err(error) = removed {
		warn!("Unable to remove offline party member {player}: {error}");
	}
}

pub(in crate::api) fn spawn_offline_sweep(state: ApiState) {
	tokio::spawn(async move {
		tokio::time::sleep(OFFLINE_GRACE).await;

		let players = PartyMembers::find()
			.inner_join(User)
			.select_only()
			.column(user::Column::MinecraftUuid)
			.into_tuple::<Uuid>()
			.all(&state.database)
			.await;

		match players {
			Ok(players) => {
				for player in players {
					expire_if_offline(&state, player).await;
				}
			}
			Err(error) => warn!("Unable to sweep offline party members: {error}"),
		}
	});
}

pub(in crate::api) async fn apply_member_state(
	state: &ApiState,
	player: Uuid,
	user_id: i32,
	member_state: MemberState,
	limited: bool,
) {
	state.party.set_state(player, member_state).await;

	// Over the limit the state is still kept, but skips the database lookup.
	let party = if limited {
		state.party.party_of(player).await
	} else {
		match lookup_party_id(state, player, Some(user_id)).await {
			Ok(party) => party,
			Err(error) => {
				warn!("Unable to look up the party of {player}: {error}");
				return;
			}
		}
	};
	let Some(party) = party else {
		return;
	};

	match state
		.party
		.schedule_forward(player, STATE_FORWARD_INTERVAL)
		.await
	{
		Forward::Now => flush_state(state, party, player).await,
		Forward::Pending => {}
		Forward::After(delay) => {
			let state = state.clone();
			tokio::spawn(async move {
				tokio::time::sleep(delay).await;
				state.party.finish_scheduled_forward(player).await;
				if let Some(party) = state.party.party_of(player).await {
					flush_state(&state, party, player).await;
				}
			});
		}
	}
}

async fn flush_state(state: &ApiState, party: Uuid, player: Uuid) {
	let Some(roster) = state.party.roster(party).await else {
		return;
	};

	let states = state.party.states_of(&roster).await;
	let transport = transport_for(state, states.values());
	if state.party.transport_changed(party, transport).await {
		broadcast_or_log(state, party).await;
	} else {
		forward_state(state, party, player).await;
	}
}

async fn forward_state(state: &ApiState, party: Uuid, player: Uuid) {
	let Some(member_state) = state.party.state_of(player).await else {
		return;
	};
	let Some(roster) = state.party.roster(party).await else {
		return;
	};

	for other in roster.into_iter().filter(|other| *other != player) {
		send_to_owner(state, other, || {
			ClientBoundPacket::PartyMemberStateChanged {
				party_id: party,
				player,
				state: member_state.clone(),
			}
		})
		.await;
	}
}
