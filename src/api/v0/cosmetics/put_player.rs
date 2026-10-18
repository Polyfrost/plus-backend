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
	state::EquipmentPersistence,
	v0::{
		account::AuthenticatedPlayer,
		cosmetics::PartialEquippedCosmetics,
		outfits::mirror_active_outfit_slots,
		websocket::announce_equipment_change,
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
	{
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

		let applied = {
			let mut player_runtime = state.realtime.player_runtime.write().await;
			let mut runtime = player_runtime.get_mut(&player.minecraft_uuid);
			let mut applied = Vec::new();
			for (slot, cosmetic_id) in &body.equipment.equipped {
				state
					.equipment_persist
					.supersede((player.minecraft_uuid, slot.clone()));
				if let Some(runtime) = runtime.as_deref_mut() {
					let previous = match cosmetic_id {
						Some(cosmetic_id) => {
							runtime.equipped.insert(slot.clone(), *cosmetic_id)
						}
						None => runtime.equipped.remove(slot),
					};
					applied.push((slot.clone(), previous, *cosmetic_id));
				}
			}
			applied
		};

		if let Err(error) = txn.commit().await {
			restore_runtime(&state, player.minecraft_uuid, applied).await;
			return Err(error.into());
		}
	}

	for (slot, cosmetic_id) in body.equipment.equipped {
		announce_equipment_change(&state, player.minecraft_uuid, slot, cosmetic_id, true)
			.await;
	}

	Ok(NoContent)
}

async fn restore_runtime(
	state: &ApiState,
	player: Uuid,
	applied: Vec<(BodySlot, Option<i32>, Option<i32>)>,
) {
	let mut player_runtime = state.realtime.player_runtime.write().await;
	let Some(runtime) = player_runtime.get_mut(&player) else {
		return;
	};
	for (slot, previous, cosmetic_id) in applied {
		if runtime.equipped.get(&slot).copied() == cosmetic_id {
			match previous {
				Some(previous) => runtime.equipped.insert(slot.clone(), previous),
				None => runtime.equipped.remove(&slot),
			};
		}
		state.equipment_persist.enqueue(EquipmentPersistence {
			player,
			cosmetic_id: runtime.equipped.get(&slot).copied(),
			slot,
		});
	}
}
