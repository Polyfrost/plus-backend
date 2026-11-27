use aide::{
	OperationIo,
	axum::{ApiRouter, routing::put_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, NoContent},
};
use std::collections::HashMap;

use entities::sea_orm_active_enums::BodySlot;
use schemars::JsonSchema;
use sea_orm::{
	ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QuerySelect, Set,
	TransactionTrait,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::{
	ApiState,
	state::{RealtimeState, holding_slot_rules},
	v0::{
		account::AuthenticatedPlayer,
		bonuses::{deliver_discoveries, record_completions, resolve_transition},
		cosmetics::PartialEquippedCosmetics,
		outfits::mirror_active_outfit_slots,
		websocket::{adopt_aura_color, announce_equipment_change},
	},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("The given ID {id} is not owned by the player")]
	UnownedCosmetic { id: i32 },
	#[error("The given ID {id} cannot be equipped in slot {slot}")]
	InvalidSlot { slot: String, id: i32 },
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("putCosmetics")
		.summary("Set a player's active cosmetics")
		.description("Sets the authorized player's active cosmetics")
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"An error given when a passed ID is not owned or cannot be equipped in a slot",
			)
			.example("The given ID 2 cannot be equipped in slot cape")
		})
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to set active \
					 cosmetics",
				)
			},
		)
}

impl IntoResponse for ResponseError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				ResponseError::UnownedCosmetic { .. }
				| ResponseError::InvalidSlot { .. } => StatusCode::BAD_REQUEST,
				ResponseError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RequestBody {
	/// Slot keyed equipment updates.
	#[serde(flatten)]
	equipment: PartialEquippedCosmetics,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/player", put_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<RequestBody>,
) -> Result<NoContent, ResponseError> {
	let updates = state.realtime.player_updates.lock(player.minecraft_uuid).await;
	let recorded = {
		use entities::{
			cosmetic_allowed_slot, player_equipped_cosmetic, player_owned_cosmetic,
			prelude::*,
		};
		use sea_orm::sea_query::{Expr, OnConflict};

		let txn = state.database.begin().await?;
		let active_outfit_id = User::find_by_id(player.id)
			.lock_exclusive()
			.one(&txn)
			.await?
			.and_then(|player| player.active_outfit_id);
		holding_slot_rules(body.equipment.equipped.values().flatten().copied())
			.all(&txn)
			.await?;

		for (slot, value) in body.equipment.equipped.clone() {
			if let Some(id) = value {
				let owned = PlayerOwnedCosmetic::find()
					.filter(player_owned_cosmetic::Column::PlayerId.eq(player.id))
					.filter(player_owned_cosmetic::Column::CosmeticId.eq(id))
					.one(&txn)
					.await?
					.is_some();
				if !owned {
					return Err(ResponseError::UnownedCosmetic { id });
				}

				let allowed = CosmeticAllowedSlot::find()
					.filter(cosmetic_allowed_slot::Column::CosmeticId.eq(id))
					.filter(cosmetic_allowed_slot::Column::Slot.eq(slot.clone()))
					.one(&txn)
					.await?
					.is_some();
				if !allowed {
					return Err(ResponseError::InvalidSlot {
						slot: format!("{slot:?}"),
						id,
					});
				}

				PlayerEquippedCosmetic::insert(player_equipped_cosmetic::ActiveModel {
					player_id: Set(player.id),
					slot: Set(slot),
					cosmetic_id: Set(id),
					updated_at: ActiveValue::NotSet,
				})
				.on_conflict(
					OnConflict::columns([
						player_equipped_cosmetic::Column::PlayerId,
						player_equipped_cosmetic::Column::Slot,
					])
					.update_column(player_equipped_cosmetic::Column::CosmeticId)
					.value(
						player_equipped_cosmetic::Column::UpdatedAt,
						Expr::current_timestamp(),
					)
					.to_owned(),
				)
				.exec(&txn)
				.await?;
			} else {
				PlayerEquippedCosmetic::delete_many()
					.filter(player_equipped_cosmetic::Column::PlayerId.eq(player.id))
					.filter(player_equipped_cosmetic::Column::Slot.eq(slot))
					.exec(&txn)
					.await?;
			}
		}

		mirror_active_outfit_slots(
			&txn,
			player.id,
			active_outfit_id,
			body.equipment.equipped.clone(),
		)
		.await?;

		let recorded = record_completions(&txn, &state.bonus_catalog, player.id).await?;

		let superseded: Vec<_> = body
			.equipment
			.equipped
			.keys()
			.filter_map(|slot| {
				state
					.equipment_persist
					.supersede((player.minecraft_uuid, slot.clone()))
			})
			.collect();
		publish_after_commit(
			&state.realtime,
			player.minecraft_uuid,
			&body.equipment.equipped,
			async move {
				txn.commit().await?;
				for supersession in superseded {
					supersession.confirm();
				}
				Ok::<_, sea_orm::DbErr>(())
			},
		)
		.await?;
		recorded
	};

	if body
		.equipment
		.equipped
		.get(&BodySlot::Aura)
		.is_some_and(Option::is_some)
	{
		adopt_aura_color(&state, player.minecraft_uuid, &updates).await;
	}
	for (slot, cosmetic_id) in body.equipment.equipped {
		announce_equipment_change(&state, player.minecraft_uuid, slot, cosmetic_id, true)
			.await;
	}
	resolve_transition(&state, player.minecraft_uuid, &updates).await;
	drop(updates);
	if recorded {
		deliver_discoveries(&state, player.minecraft_uuid, player.id).await;
	}

	Ok(NoContent)
}

async fn publish_after_commit<E>(
	realtime: &RealtimeState,
	player: Uuid,
	equipment: &HashMap<BodySlot, Option<i32>>,
	commit: impl Future<Output = Result<(), E>>,
) -> Result<(), E> {
	commit.await?;
	let mut player_runtime = realtime.player_runtime.write().await;
	if let Some(runtime) = player_runtime.get_mut(&player) {
		for (slot, cosmetic_id) in equipment {
			match cosmetic_id {
				Some(cosmetic_id) => runtime.equipped.insert(slot.clone(), *cosmetic_id),
				None => runtime.equipped.remove(slot),
			};
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use entities::sea_orm_active_enums::BodySlot;
	use uuid::Uuid;

	use super::publish_after_commit;
	use crate::api::state::{PlayerRuntimeState, RealtimeState};

	#[tokio::test]
	async fn the_live_equipment_changes_only_after_the_commit_lands() {
		let realtime = RealtimeState::default();
		let player = Uuid::new_v4();
		let cape = HashMap::from([(BodySlot::Cape, 10)]);
		let live = PlayerRuntimeState { equipped: cape.clone(), ..Default::default() };
		realtime.player_runtime.write().await.insert(player, live);
		let equipped = async || realtime.player_runtime.read().await[&player].equipped.clone();
		let change = HashMap::from([(BodySlot::Hat, Some(41)), (BodySlot::Cape, None)]);

		let failed = publish_after_commit(&realtime, player, &change, async {
			assert_eq!(equipped().await, cape);
			Err::<(), _>("commit failed")
		});
		assert!(failed.await.is_err());
		assert_eq!(equipped().await, cape);

		publish_after_commit(&realtime, player, &change, async { Ok::<_, ()>(()) }).await.unwrap();
		assert_eq!(equipped().await, HashMap::from([(BodySlot::Hat, 41)]));

		let offline = Uuid::new_v4();
		publish_after_commit(&realtime, offline, &change, async { Ok::<_, ()>(()) }).await.unwrap();
		assert!(!realtime.player_runtime.read().await.contains_key(&offline));
	}
}
