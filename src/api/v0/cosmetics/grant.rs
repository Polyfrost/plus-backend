use std::collections::{HashMap, HashSet};

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use entities::{
	cosmetic, ownership_grant,
	prelude::*,
	sea_orm_active_enums::{
		CosmeticType, OwnershipEventKind, TransactionProvider, TransactionStatus,
	},
	transaction, user,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Set,
	TransactionTrait, prelude::Expr,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{
	api::{
		ApiState, error_response,
		permissions::{Require, perm},
		v0::websocket::{send_to_owner, structs::ClientBoundPacket, unequip_revoked},
	},
	database::{DatabaseUserExt, record_ownership_events},
	ownership::{self, Settled},
};

const MAX_PLAYERS: usize = 500;
const MAX_COSMETICS: usize = 100;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum GrantError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("At most {MAX_PLAYERS} players and {MAX_COSMETICS} cosmetics per request")]
	TooMany,
	#[error("Unable to query database: {0}")]
	Database(#[from] DbErr),
}

impl IntoResponse for GrantError {
	fn into_response(self) -> Response {
		error_response(
			match self {
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::TooMany => StatusCode::BAD_REQUEST,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("grantCosmetic")
		.summary("Grant a cosmetic to a player")
		.description("Grants cosmetic ownership to a player. Admin role required.")
		.tag("cosmetics")
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrantRequest {
	player: Uuid,
	cosmetic_id: i32,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/grant", post_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: Require<{ perm::GRANT_COSMETICS }>,
	Json(body): Json<GrantRequest>,
) -> Result<StatusCode, GrantError> {
	grant_cosmetics(&state, &[body.player], &[body.cosmetic_id]).await?;

	Ok(StatusCode::NO_CONTENT)
}

/// Grants every cosmetic to every player and tells any connected client.
/// Returns the granted ids, each group expanded to all its variants.
pub(in crate::api) async fn grant_cosmetics(
	state: &ApiState,
	players: &[Uuid],
	cosmetic_ids: &[i32],
) -> Result<Vec<i32>, GrantError> {
	check_size(players, cosmetic_ids)?;
	let players: HashSet<Uuid> = players.iter().copied().collect();

	let txn = state.database.begin().await?;
	let cosmetics = expand_groups(&txn, cosmetic_ids).await?;
	let mut ids: Vec<i32> = cosmetics.iter().map(|cosmetic| cosmetic.id).collect();
	ids.sort_unstable();

	if !ids.is_empty() {
		for &player_uuid in &players {
			let player = User::get_or_create(&txn, player_uuid).await?;
			let transaction = transaction::ActiveModel {
				player_id: Set(player.id),
				provider: Set(TransactionProvider::AdminGrant),
				provider_transaction_id: Set(None),
				status: Set(TransactionStatus::Completed),
				raw_metadata: Set(json!({ "reason": "admin_grant" })),
				..Default::default()
			}
			.insert(&txn)
			.await?;

			// Owned for good, including over a rental the player already holds.
			ownership::record(
				&txn,
				player.id,
				&ids,
				TransactionProvider::AdminGrant,
				Some(transaction.id),
				None,
				None,
			)
			.await?;
			ownership::settle(&txn, player.id, &ids).await?;

			record_ownership_events(
				&txn,
				player.id,
				&ids,
				OwnershipEventKind::Granted,
				TransactionProvider::AdminGrant,
				Some(transaction.id),
				None,
				None,
			)
			.await?;
		}
	}

	txn.commit().await?;

	if !ids.is_empty() {
		let (cosmetic_ids, emote_ids) = split_emotes(cosmetics.iter());
		for player in players {
			notify(state, player, &cosmetic_ids, &emote_ids, false).await;
		}
	}

	Ok(ids)
}

/// Takes the cosmetics away from each player, whatever granted them, paid
/// grants included. Returns what each player lost; players who never logged
/// in or owned none of it are left out.
pub(in crate::api) async fn revoke_cosmetics(
	state: &ApiState,
	players: &[Uuid],
	cosmetic_ids: &[i32],
) -> Result<HashMap<Uuid, Vec<i32>>, GrantError> {
	check_size(players, cosmetic_ids)?;

	let txn = state.database.begin().await?;
	let cosmetics: HashMap<i32, cosmetic::Model> = expand_groups(&txn, cosmetic_ids)
		.await?
		.into_iter()
		.map(|cosmetic| (cosmetic.id, cosmetic))
		.collect();
	let ids: Vec<i32> = cosmetics.keys().copied().collect();

	let found = if ids.is_empty() {
		Vec::new()
	} else {
		User::find()
			.filter(user::Column::MinecraftUuid.is_in(players.iter().copied()))
			.all(&txn)
			.await?
	};

	let mut removed = HashMap::new();
	for player in found {
		OwnershipGrant::update_many()
			.col_expr(ownership_grant::Column::Active, Expr::value(false))
			.filter(ownership_grant::Column::PlayerId.eq(player.id))
			.filter(ownership_grant::Column::CosmeticId.is_in(ids.clone()))
			.filter(ownership_grant::Column::Active.eq(true))
			.exec(&txn)
			.await?;

		let mut gone: Vec<i32> = ownership::settle(&txn, player.id, &ids)
			.await?
			.into_iter()
			.filter(|(_, settled)| *settled == Settled::Removed)
			.map(|(id, _)| id)
			.collect();
		if gone.is_empty() {
			continue;
		}
		gone.sort_unstable();

		record_ownership_events(
			&txn,
			player.id,
			&gone,
			OwnershipEventKind::Revoked,
			TransactionProvider::AdminGrant,
			None,
			None,
			None,
		)
		.await?;
		removed.insert(player.minecraft_uuid, gone);
	}

	txn.commit().await?;

	let split: HashMap<Uuid, (Vec<i32>, Vec<i32>)> = removed
		.iter()
		.map(|(player, gone)| {
			(*player, split_emotes(gone.iter().filter_map(|id| cosmetics.get(id))))
		})
		.collect();
	let unequip = split
		.iter()
		.map(|(player, (cosmetic_ids, _))| (*player, cosmetic_ids.clone()))
		.collect();
	unequip_revoked(state, &unequip).await;
	for (player, (cosmetic_ids, emote_ids)) in split {
		notify(state, player, &cosmetic_ids, &emote_ids, true).await;
	}

	Ok(removed)
}

fn check_size(players: &[Uuid], cosmetic_ids: &[i32]) -> Result<(), GrantError> {
	if players.len() > MAX_PLAYERS || cosmetic_ids.len() > MAX_COSMETICS {
		return Err(GrantError::TooMany);
	}
	Ok(())
}

/// The cosmetics behind these ids, each grouped one bringing its whole group.
async fn expand_groups(
	db: &impl ConnectionTrait,
	cosmetic_ids: &[i32],
) -> Result<Vec<cosmetic::Model>, GrantError> {
	let requested: HashSet<i32> = cosmetic_ids.iter().copied().collect();
	if requested.is_empty() {
		return Ok(Vec::new());
	}

	let found = Cosmetic::find()
		.filter(cosmetic::Column::Id.is_in(requested.iter().copied()))
		.all(db)
		.await?;
	if found.len() != requested.len() {
		return Err(GrantError::MissingCosmetic);
	}

	let groups: HashSet<i32> = found.iter().filter_map(|cosmetic| cosmetic.group_id).collect();
	let mut cosmetics: HashMap<i32, cosmetic::Model> = if groups.is_empty() {
		HashMap::new()
	} else {
		Cosmetic::find()
			.filter(cosmetic::Column::GroupId.is_in(groups))
			.all(db)
			.await?
			.into_iter()
			.map(|cosmetic| (cosmetic.id, cosmetic))
			.collect()
	};
	cosmetics.extend(found.into_iter().map(|cosmetic| (cosmetic.id, cosmetic)));

	Ok(cosmetics.into_values().collect())
}

fn split_emotes<'a>(cosmetics: impl Iterator<Item = &'a cosmetic::Model>) -> (Vec<i32>, Vec<i32>) {
	let (emotes, cosmetics): (Vec<_>, Vec<_>) =
		cosmetics.partition(|cosmetic| matches!(cosmetic.r#type, CosmeticType::Emote));
	(
		cosmetics.into_iter().map(|cosmetic| cosmetic.id).collect(),
		emotes.into_iter().map(|cosmetic| cosmetic.id).collect(),
	)
}

async fn notify(
	state: &ApiState,
	player: Uuid,
	cosmetic_ids: &[i32],
	emote_ids: &[i32],
	revoked: bool,
) {
	send_to_owner(state, player, || ClientBoundPacket::OwnershipUpdated {
		player,
		cosmetic_ids: cosmetic_ids.to_vec(),
		emote_ids: emote_ids.to_vec(),
		revoked,
	})
	.await;
}
