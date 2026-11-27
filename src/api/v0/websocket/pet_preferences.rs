use std::collections::{HashMap, HashSet};

use chrono::Utc;
use entities::{
	player_pet_preferences, prelude::*, sea_orm_active_enums::PetInteractions, user,
};
use sea_orm::{
	ColumnTrait as _, ConnectionTrait, DbErr, EntityTrait as _, QueryFilter as _, Set,
	sea_query::OnConflict,
};
use uuid::Uuid;

use super::structs::PetPlay;

impl From<PetInteractions> for PetPlay {
	fn from(stored: PetInteractions) -> Self {
		match stored {
			PetInteractions::Everyone => Self::Everyone,
			PetInteractions::Friends => Self::Friends,
			PetInteractions::Off => Self::Off,
		}
	}
}

impl From<PetPlay> for PetInteractions {
	fn from(play: PetPlay) -> Self {
		match play {
			PetPlay::Everyone => Self::Everyone,
			PetPlay::Friends => Self::Friends,
			PetPlay::Off => Self::Off,
		}
	}
}

pub(crate) fn plan_change(current: PetPlay, requested: PetPlay) -> Option<PetPlay> {
	(current != requested).then_some(requested)
}

pub(crate) fn stored_play(stored: Option<&player_pet_preferences::Model>) -> PetPlay {
	stored.map_or(PetPlay::Everyone, |row| {
		row.play
			.map_or_else(|| PetPlay::from_switch(row.interactions), PetPlay::from)
	})
}

pub(crate) async fn load_play(
	db: &impl ConnectionTrait,
	player_id: i32,
) -> Result<PetPlay, DbErr> {
	Ok(stored_play(
		PlayerPetPreferences::find_by_id(player_id)
			.one(db)
			.await?
			.as_ref(),
	))
}

pub(crate) async fn load_players_play(
	db: &impl ConnectionTrait,
	players: &[Uuid],
) -> Result<HashMap<Uuid, PetPlay>, DbErr> {
	if players.is_empty() {
		return Ok(HashMap::new());
	}

	Ok(PlayerPetPreferences::find()
		.find_also_related(User)
		.filter(user::Column::MinecraftUuid.is_in(players.to_vec()))
		.filter(
			player_pet_preferences::Column::Interactions
				.eq(false)
				.or(player_pet_preferences::Column::Play.is_not_null()),
		)
		.all(db)
		.await?
		.into_iter()
		.filter_map(|(row, user)| {
			let play = stored_play(Some(&row));
			user.filter(|_| play != PetPlay::Everyone)
				.map(|user| (user.minecraft_uuid, play))
		})
		.collect())
}

pub(crate) async fn store_play(
	db: &impl ConnectionTrait,
	player_id: i32,
	play: PetPlay,
) -> Result<(), DbErr> {
	PlayerPetPreferences::insert(player_pet_preferences::ActiveModel {
		player_id: Set(player_id),
		interactions: Set(play.allows_interactions()),
		play: Set(Some(play.into())),
		updated_at: Set(Utc::now().into()),
	})
	.on_conflict(
		OnConflict::column(player_pet_preferences::Column::PlayerId)
			.update_columns([
				player_pet_preferences::Column::Interactions,
				player_pet_preferences::Column::Play,
				player_pet_preferences::Column::UpdatedAt,
			])
			.to_owned(),
	)
	.exec(db)
	.await?;
	Ok(())
}

#[derive(Debug, Default)]
pub(crate) struct Friendships(HashSet<(Uuid, Uuid)>);

impl Friendships {
	fn key(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
		if a <= b { (a, b) } else { (b, a) }
	}

	pub(crate) fn new(pairs: impl IntoIterator<Item = (Uuid, Uuid)>) -> Self {
		Self(pairs.into_iter().map(|(a, b)| Self::key(a, b)).collect())
	}

	pub(crate) fn are_friends(&self, a: Uuid, b: Uuid) -> bool {
		a != b && self.0.contains(&Self::key(a, b))
	}
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FriendScope {
	pub focus: Vec<Uuid>,
	pub others: Vec<Uuid>,
}

fn sorted(players: impl IntoIterator<Item = Uuid>) -> Vec<Uuid> {
	let mut players = players
		.into_iter()
		.collect::<HashSet<_>>()
		.into_iter()
		.collect::<Vec<_>>();
	players.sort_unstable();
	players
}

fn candidates(new: &[Uuid], subscribed: &HashSet<Uuid>, watcher: Uuid) -> Vec<Uuid> {
	sorted(subscribed.iter().chain(new).copied().chain([watcher]))
}

fn already_subscribed<'a>(
	new: &'a [Uuid],
	subscribed: &'a HashSet<Uuid>,
) -> impl Iterator<Item = Uuid> + 'a {
	subscribed
		.iter()
		.copied()
		.filter(|player| !new.contains(player))
}

pub(crate) fn snapshot_friend_scope(
	new: &[Uuid],
	subscribed: &HashSet<Uuid>,
	watcher: Uuid,
	play_of: impl Fn(Uuid) -> PetPlay,
) -> Option<FriendScope> {
	if new.is_empty() {
		return None;
	}
	if new.iter().any(|player| play_of(*player) == PetPlay::Friends) {
		return Some(FriendScope {
			focus: sorted(new.iter().copied()),
			others: candidates(new, subscribed, watcher),
		});
	}
	let picky = sorted(
		already_subscribed(new, subscribed)
			.filter(|player| play_of(*player) == PetPlay::Friends),
	);
	(!picky.is_empty()).then(|| FriendScope {
		focus: picky,
		others: sorted(new.iter().copied()),
	})
}

pub(crate) fn snapshot_friends(
	new: &[Uuid],
	subscribed: &HashSet<Uuid>,
	watcher: Uuid,
	play_of: impl Fn(Uuid) -> PetPlay,
	friendships: &Friendships,
) -> HashMap<Uuid, Vec<Uuid>> {
	let everyone = candidates(new, subscribed, watcher);
	let newcomers = sorted(new.iter().copied());
	let mut lists = HashMap::new();
	for player in &newcomers {
		if play_of(*player) != PetPlay::Friends {
			continue;
		}
		let friends = everyone
			.iter()
			.copied()
			.filter(|other| friendships.are_friends(*player, *other))
			.collect();
		lists.insert(*player, friends);
	}
	for player in already_subscribed(new, subscribed) {
		if play_of(player) != PetPlay::Friends {
			continue;
		}
		let friends = newcomers
			.iter()
			.copied()
			.filter(|other| friendships.are_friends(player, *other))
			.collect::<Vec<_>>();
		if !friends.is_empty() {
			lists.insert(player, friends);
		}
	}
	lists
}

pub(crate) fn watcher_friends(
	player: Uuid,
	play: PetPlay,
	subscriptions: &HashSet<Uuid>,
	watcher: Uuid,
	friends: &HashSet<Uuid>,
) -> Vec<Uuid> {
	if play != PetPlay::Friends {
		return Vec::new();
	}
	sorted(
		subscriptions
			.iter()
			.copied()
			.chain([watcher])
			.filter(|other| *other != player && friends.contains(other)),
	)
}

#[cfg(test)]
mod tests {
	use std::collections::{HashMap, HashSet};

	use chrono::Utc;
	use entities::{player_pet_preferences, sea_orm_active_enums::PetInteractions};
	use uuid::Uuid;

	use super::{
		FriendScope, Friendships, plan_change, snapshot_friend_scope, snapshot_friends,
		stored_play, watcher_friends,
	};
	use crate::api::v0::websocket::structs::PetPlay;

	fn ids<const N: usize>() -> [Uuid; N] {
		std::array::from_fn(|index| Uuid::from_u128(index as u128 + 1))
	}

	fn plays(entries: &[(Uuid, PetPlay)]) -> impl Fn(Uuid) -> PetPlay + use<> {
		let entries = entries.iter().copied().collect::<HashMap<_, _>>();
		move |player| entries.get(&player).copied().unwrap_or_default()
	}

	#[test]
	fn the_stored_setting_wins_over_the_old_switch_and_repeats_change_nothing() {
		let row = |interactions, play| player_pet_preferences::Model { player_id: 1, interactions, play, updated_at: Utc::now().into() };
		assert_eq!(stored_play(None), PetPlay::Everyone);
		for (interactions, play, expected) in [
			(true, None, PetPlay::Everyone),
			(false, None, PetPlay::Off),
			(true, Some(PetInteractions::Friends), PetPlay::Friends),
			(false, Some(PetInteractions::Everyone), PetPlay::Everyone),
			(true, Some(PetInteractions::Off), PetPlay::Off),
		] {
			assert_eq!(stored_play(Some(&row(interactions, play))), expected);
		}
		for play in [PetPlay::Everyone, PetPlay::Friends, PetPlay::Off] {
			assert_eq!(PetPlay::from(PetInteractions::from(play)), play);
		}
		assert_eq!(plan_change(PetPlay::Everyone, PetPlay::Off), Some(PetPlay::Off));
		assert_eq!(plan_change(PetPlay::Off, PetPlay::Friends), Some(PetPlay::Friends));
		assert_eq!(plan_change(PetPlay::Friends, PetPlay::Friends), None);
		assert_eq!(plan_change(PetPlay::Everyone, PetPlay::Everyone), None);
	}

	#[test]
	fn snapshots_list_friends_only_for_friends_only_players() {
		let [a, b, c] = ids();
		let friendships = Friendships::new([(b, a)]);
		assert!(friendships.are_friends(a, b) && friendships.are_friends(b, a));
		assert!(!friendships.are_friends(a, c) && !Friendships::new([(a, a)]).are_friends(a, a));

		let [watcher, picky, old_friend, stranger, new_friend] = ids();
		let subscribed = HashSet::from([picky, old_friend, stranger, new_friend]);
		let new = [picky, new_friend];
		let play_of = plays(&[(picky, PetPlay::Friends)]);
		let friendships = Friendships::new([(picky, old_friend), (picky, new_friend), (picky, watcher), (stranger, old_friend)]);
		let scope = FriendScope { focus: vec![picky, new_friend], others: vec![watcher, picky, old_friend, stranger, new_friend] };
		assert_eq!(snapshot_friend_scope(&new, &subscribed, watcher, &play_of), Some(scope));
		let expected = HashMap::from([(picky, vec![watcher, old_friend, new_friend])]);
		assert_eq!(snapshot_friends(&new, &subscribed, watcher, &play_of, &friendships), expected);
		let alone = snapshot_friends(&[picky], &HashSet::from([picky, stranger]), watcher, &play_of, &Friendships::default());
		assert_eq!(alone, HashMap::from([(picky, Vec::new())]));

		let [watcher, picky, old_friend, newcomer, new_stranger] = ids();
		let subscribed = HashSet::from([picky, old_friend, newcomer, new_stranger]);
		let new = [newcomer, new_stranger];
		let play_of = plays(&[(picky, PetPlay::Friends), (old_friend, PetPlay::Friends)]);
		let friendships = Friendships::new([(picky, newcomer), (picky, old_friend)]);
		let scope = FriendScope { focus: vec![picky, old_friend], others: vec![newcomer, new_stranger] };
		assert_eq!(snapshot_friend_scope(&new, &subscribed, watcher, &play_of), Some(scope));
		let expected = HashMap::from([(picky, vec![newcomer])]);
		assert_eq!(snapshot_friends(&new, &subscribed, watcher, &play_of, &friendships), expected);

		let [watcher, open, quiet, old] = ids();
		let subscribed = HashSet::from([open, quiet, old]);
		let play_of = plays(&[(quiet, PetPlay::Off)]);
		assert_eq!(snapshot_friend_scope(&[open, quiet], &subscribed, watcher, &play_of), None);
		assert_eq!(snapshot_friend_scope(&[], &subscribed, watcher, &play_of), None);
		let friendships = Friendships::new([(open, quiet), (open, watcher)]);
		assert!(snapshot_friends(&[open, quiet], &subscribed, watcher, &play_of, &friendships).is_empty());
	}

	#[test]
	fn a_preference_change_lists_friends_per_watcher() {
		let [picky, first, second, near_first, near_both, far] = ids();
		let friends = HashSet::from([first, near_first, near_both, far]);
		let first_sees = HashSet::from([picky, near_first, near_both]);
		let second_sees = HashSet::from([picky, near_both]);
		let friends_only = |sees, watcher| watcher_friends(picky, PetPlay::Friends, sees, watcher, &friends);
		assert_eq!(friends_only(&first_sees, first), vec![first, near_first, near_both]);
		assert_eq!(friends_only(&second_sees, second), vec![near_both]);
		for play in [PetPlay::Everyone, PetPlay::Off] {
			assert!(watcher_friends(picky, play, &first_sees, first, &friends).is_empty());
		}
	}
}
