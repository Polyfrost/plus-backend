mod daily;
mod kinds;
mod requirements;
mod rewards;

use std::collections::{HashMap, HashSet};

use aide::{
	OperationIo,
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, State},
	http::StatusCode,
	response::{IntoResponse, Response},
};
use chrono::Utc;
use entities::{event, event_claim, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, DbErr, EntityTrait,
	QueryFilter, QueryOrder, Set, SqlErr, TransactionTrait, prelude::DateTimeWithTimeZone,
};
use serde::{Deserialize, Serialize};
use serde_json::{Error as JsonError, from_value, json, to_value};
use uuid::Uuid;

use self::{
	kinds::EventKind,
	requirements::{Progress, progress},
	rewards::{Granted, Reward, deliver, grant},
};
use crate::api::{
	ApiState, error_response,
	permissions::{Require, perm},
	v0::{
		account::{AuthenticatedPlayer, ClientKind},
		cosmetics::grant::GrantError,
	},
	v1::discounts::DiscountError,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum EventError {
	#[error("No event with that id exists")]
	NotFound,
	#[error("That event has no such reward")]
	NoSlot,
	#[error("{0}")]
	Invalid(&'static str),
	#[error("That reward is not available yet")]
	NotYetAvailable,
	#[error("That reward can no longer be claimed")]
	Expired,
	#[error("That reward was already claimed")]
	AlreadyClaimed,
	#[error("That reward's requirements are not met yet")]
	RequirementsNotMet,
	#[error("Events can only be used in-game")]
	NotInGame,
	#[error("Stored event is malformed: {0}")]
	Malformed(#[from] JsonError),
	#[error(transparent)]
	Grant(#[from] GrantError),
	#[error(transparent)]
	Coupon(#[from] DiscountError),
	#[error("Database error: {0}")]
	Database(#[from] DbErr),
}

impl IntoResponse for EventError {
	fn into_response(self) -> Response {
		let status = match self {
			Self::Grant(error) => return error.into_response(),
			Self::Coupon(error) => return error.into_response(),
			Self::NotFound | Self::NoSlot => StatusCode::NOT_FOUND,
			Self::Invalid(_) | Self::NotYetAvailable | Self::RequirementsNotMet => {
				StatusCode::BAD_REQUEST
			}
			Self::Expired => StatusCode::GONE,
			Self::AlreadyClaimed => StatusCode::CONFLICT,
			Self::NotInGame => StatusCode::FORBIDDEN,
			Self::Malformed(_) | Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
		};
		error_response(status, self)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EventBody {
	name: String,
	#[serde(flatten)]
	config: EventKind,
}

#[derive(Debug, Serialize, JsonSchema)]
struct EventView {
	id: i32,
	name: String,
	/// When the first reward opens.
	starts_at: DateTimeWithTimeZone,
	/// When the last reward closes.
	ends_at: DateTimeWithTimeZone,
	#[serde(flatten)]
	config: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SlotStatus {
	Upcoming,
	Claimable,
	Claimed,
	Missed,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SlotView {
	/// What to claim it by, e.g. `2026-12-01` for a daily reward.
	slot: String,
	opens_at: DateTimeWithTimeZone,
	closes_at: DateTimeWithTimeZone,
	status: SlotStatus,
	/// Hidden until the slot opens.
	reward: Option<Reward>,
	/// What the player received, once claimed.
	granted: Option<Granted>,
	/// All must be met to claim.
	requirements: Vec<Progress>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PlayerEventView {
	id: i32,
	name: String,
	starts_at: DateTimeWithTimeZone,
	ends_at: DateTimeWithTimeZone,
	slots: Vec<SlotView>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ClaimView {
	player: Uuid,
	username: Option<String>,
	slot: String,
	granted: Granted,
	claimed_at: DateTimeWithTimeZone,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route(
			"/admin/events",
			get_with(self::list, self::list_doc).post_with(self::create, self::create_doc),
		)
		.api_route(
			"/admin/events/{id}",
			post_with(self::update, self::update_doc).delete_with(self::remove, self::remove_doc),
		)
		.api_route("/admin/events/{id}/claims", get_with(self::claims, self::claims_doc))
		.api_route("/events", get_with(self::active, self::active_doc))
		.api_route("/events/{id}/claim/{slot}", post_with(self::claim, self::claim_doc))
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listEvents")
		.summary("List events")
		.description("Lists every event, past and future, with its settings. Requires `MANAGE_EVENTS`.")
		.tag("events")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createEvent")
		.summary("Create an event")
		.description(
			"Creates an event of the given `kind`. Each kind turns its settings \
			 into rewards a player can claim once while they are open, and the \
			 event runs from the first opening to the last closing. A \
			 `daily_rewards` event opens one reward per UTC day for \
			 `claim_window_hours`. Requires `MANAGE_EVENTS`.",
		)
		.tag("events")
}

fn update_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateEvent")
		.summary("Replace an event")
		.description(
			"Replaces an event's name and settings. Claims are kept against \
			 their slot, so a slot already claimed stays claimed. Requires \
			 `MANAGE_EVENTS`.",
		)
		.tag("events")
}

fn remove_doc(op: TransformOperation) -> TransformOperation {
	op.id("deleteEvent")
		.summary("Delete an event")
		.description(
			"Deletes an event and its claims. What was already granted, \
			 cosmetics and coupons, is kept. Requires `MANAGE_EVENTS`.",
		)
		.tag("events")
}

fn claims_doc(op: TransformOperation) -> TransformOperation {
	op.id("listEventClaims")
		.summary("List an event's claims")
		.description(
			"Lists who claimed what, oldest first, for fulfilling external \
			 rewards. Requires `MANAGE_EVENTS`.",
		)
		.tag("events")
}

fn active_doc(op: TransformOperation) -> TransformOperation {
	op.id("listActiveEvents")
		.summary("List running events")
		.description(
			"Lists events that have started and still have something open, \
			 with the caller's status for each reward. Game clients only.",
		)
		.tag("events")
}

fn claim_doc(op: TransformOperation) -> TransformOperation {
	op.id("claimEventReward")
		.summary("Claim a reward")
		.description(
			"Claims one open reward of an event by its `slot`, returning what \
			 was granted. Game clients only.",
		)
		.tag("events")
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn list(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_EVENTS }>,
) -> Result<Json<Vec<EventView>>, EventError> {
	Event::find()
		.order_by_desc(event::Column::StartsAt)
		.all(&state.database)
		.await?
		.into_iter()
		.map(view)
		.collect::<Result<_, _>>()
		.map(Json)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn create(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_EVENTS }>,
	Json(body): Json<EventBody>,
) -> Result<Json<EventView>, EventError> {
	let event = checked_row(&state.database, &body, ActiveValue::NotSet)
		.await?
		.insert(&state.database)
		.await?;

	Ok(Json(view(event)?))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn update(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_EVENTS }>,
	Path(id): Path<i32>,
	Json(body): Json<EventBody>,
) -> Result<Json<EventView>, EventError> {
	let event = Event::update(checked_row(&state.database, &body, ActiveValue::Unchanged(id)).await?)
		.exec(&state.database)
		.await
		.map_err(|error| match error {
			DbErr::RecordNotUpdated => EventError::NotFound,
			error => error.into(),
		})?;

	Ok(Json(view(event)?))
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn remove(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_EVENTS }>,
	Path(id): Path<i32>,
) -> Result<StatusCode, EventError> {
	let result = Event::delete_by_id(id).exec(&state.database).await?;
	if result.rows_affected == 0 {
		return Err(EventError::NotFound);
	}

	Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn claims(
	State(state): State<ApiState>,
	_auth: Require<{ perm::MANAGE_EVENTS }>,
	Path(id): Path<i32>,
) -> Result<Json<Vec<ClaimView>>, EventError> {
	EventClaim::find()
		.filter(event_claim::Column::EventId.eq(id))
		.find_also_related(User)
		.order_by_asc(event_claim::Column::ClaimedAt)
		.all(&state.database)
		.await?
		.into_iter()
		.filter_map(|(claim, player)| Some((claim, player?)))
		.map(|(claim, player)| {
			Ok(ClaimView {
				player: player.minecraft_uuid,
				username: player.username,
				slot: claim.slot,
				granted: from_value(claim.granted)?,
				claimed_at: claim.claimed_at,
			})
		})
		.collect::<Result<_, _>>()
		.map(Json)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn active(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	client: ClientKind,
) -> Result<Json<Vec<PlayerEventView>>, EventError> {
	if client != ClientKind::Game {
		return Err(EventError::NotInGame);
	}

	let now = Utc::now().fixed_offset();
	let events = Event::find()
		.filter(event::Column::StartsAt.lte(now))
		.filter(event::Column::EndsAt.gt(now))
		.order_by_asc(event::Column::StartsAt)
		.all(&state.database)
		.await?;
	let mut claimed: HashMap<(i32, String), Granted> = EventClaim::find()
		.filter(event_claim::Column::UserId.eq(player.id))
		.filter(event_claim::Column::EventId.is_in(events.iter().map(|event| event.id)))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|claim| Ok(((claim.event_id, claim.slot), from_value(claim.granted)?)))
		.collect::<Result<_, EventError>>()?;

	let mut views = Vec::with_capacity(events.len());
	for event in events {
		let config: EventKind = from_value(event.config)?;
		let mut slots = Vec::new();
		for slot in config.slots() {
			let granted = claimed.remove(&(event.id, slot.key.clone()));
			let status = status(slot.opens_at, slot.closes_at, granted.is_some(), now);
			let window = (status == SlotStatus::Claimable).then_some((slot.opens_at, now));
			slots.push(SlotView {
				reward: (status != SlotStatus::Upcoming).then(|| slot.reward.clone()),
				requirements: progress(&state.database, player.id, slot.requirements, window)
					.await?,
				slot: slot.key,
				opens_at: slot.opens_at,
				closes_at: slot.closes_at,
				status,
				granted,
			});
		}
		views.push(PlayerEventView {
			id: event.id,
			name: event.name,
			starts_at: event.starts_at,
			ends_at: event.ends_at,
			slots,
		});
	}

	Ok(Json(views))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn claim(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	client: ClientKind,
	Path((id, key)): Path<(i32, String)>,
) -> Result<Json<Granted>, EventError> {
	if client != ClientKind::Game {
		return Err(EventError::NotInGame);
	}

	let event = Event::find_by_id(id)
		.one(&state.database)
		.await?
		.ok_or(EventError::NotFound)?;
	let config: EventKind = from_value(event.config)?;
	let slot = config
		.slots()
		.into_iter()
		.find(|slot| slot.key == key)
		.ok_or(EventError::NoSlot)?;
	let now = Utc::now().fixed_offset();
	match status(slot.opens_at, slot.closes_at, false, now) {
		SlotStatus::Upcoming => return Err(EventError::NotYetAvailable),
		SlotStatus::Missed => return Err(EventError::Expired),
		SlotStatus::Claimable | SlotStatus::Claimed => {}
	}
	let window = Some((slot.opens_at, now));
	let progress = progress(&state.database, player.id, slot.requirements, window).await?;
	if !progress.iter().all(|requirement| requirement.met) {
		return Err(EventError::RequirementsNotMet);
	}

	let txn = state.database.begin().await?;
	let metadata = json!({ "reason": "event", "event_id": id, "slot": key });
	let granted = grant(&txn, &player, slot.reward, metadata).await?;
	EventClaim::insert(event_claim::ActiveModel {
		event_id: Set(id),
		user_id: Set(player.id),
		slot: Set(key),
		granted: Set(to_value(&granted)?),
		claimed_at: ActiveValue::NotSet,
	})
	.exec_without_returning(&txn)
	.await
	.map_err(|error| match error.sql_err() {
		Some(SqlErr::UniqueConstraintViolation(_)) => EventError::AlreadyClaimed,
		_ => error.into(),
	})?;
	txn.commit().await?;

	deliver(&state, player.minecraft_uuid, slot.reward, &granted).await;

	Ok(Json(granted))
}

fn view(event: event::Model) -> Result<EventView, EventError> {
	Ok(EventView {
		id: event.id,
		name: event.name,
		starts_at: event.starts_at,
		ends_at: event.ends_at,
		config: from_value(event.config)?,
	})
}

fn status(
	opens_at: DateTimeWithTimeZone,
	closes_at: DateTimeWithTimeZone,
	claimed: bool,
	now: DateTimeWithTimeZone,
) -> SlotStatus {
	if claimed {
		SlotStatus::Claimed
	} else if now < opens_at {
		SlotStatus::Upcoming
	} else if now < closes_at {
		SlotStatus::Claimable
	} else {
		SlotStatus::Missed
	}
}

/// The row for `body` once what every kind shares checks out.
async fn checked_row(
	db: &impl ConnectionTrait,
	body: &EventBody,
	id: ActiveValue<i32>,
) -> Result<event::ActiveModel, EventError> {
	if body.name.trim().is_empty() {
		return Err(EventError::Invalid("Name must not be empty"));
	}

	let slots = body.config.slots();
	let mut keys = HashSet::new();
	for slot in &slots {
		if !keys.insert(slot.key.as_str()) {
			return Err(EventError::Invalid("Two rewards share a slot"));
		}
		if slot.closes_at <= slot.opens_at {
			return Err(EventError::Invalid("Every reward must stay open for some time"));
		}
		for requirement in slot.requirements {
			requirement.validate().map_err(EventError::Invalid)?;
		}
	}
	let (Some(starts_at), Some(ends_at)) = (
		slots.iter().map(|slot| slot.opens_at).min(),
		slots.iter().map(|slot| slot.closes_at).max(),
	) else {
		return Err(EventError::Invalid("An event needs at least one reward"));
	};

	rewards::validate(db, slots.iter().map(|slot| slot.reward)).await?;

	Ok(event::ActiveModel {
		id,
		name: Set(body.name.trim().to_owned()),
		starts_at: Set(starts_at),
		ends_at: Set(ends_at),
		config: Set(to_value(&body.config)?),
		created_at: ActiveValue::NotSet,
	})
}

#[cfg(test)]
mod tests {
	use chrono::Duration;
	use serde_json::{from_value, json};

	use super::{SlotStatus, kinds::EventKind, status};

	#[test]
	fn a_daily_reward_stays_claimable_for_its_window() {
		let config: EventKind = from_value(json!({
			"kind": "daily_rewards",
			"claim_window_hours": 72,
			"rewards": [{ "day": "2026-12-01", "reward": { "type": "cosmetic", "cosmetic_id": 1 } }],
		}))
		.unwrap();
		let slots = config.slots();
		let slot = &slots[0];
		let at = |hours| slot.opens_at + Duration::hours(hours);
		let check = |hours, claimed| status(slot.opens_at, slot.closes_at, claimed, at(hours));

		assert_eq!(slot.key, "2026-12-01");
		assert_eq!(check(-1, false), SlotStatus::Upcoming);
		assert_eq!(check(0, false), SlotStatus::Claimable);
		assert_eq!(check(71, false), SlotStatus::Claimable, "late on the 3rd");
		assert_eq!(check(72, false), SlotStatus::Missed);
		assert_eq!(check(80, true), SlotStatus::Claimed);
	}
}
