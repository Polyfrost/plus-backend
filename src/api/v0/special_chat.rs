use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State};
use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Serialize;

use crate::api::{
	ApiState,
	v0::account::AuthenticatedPlayer,
	v0::groups::{
		GroupError, find_user_by_uuid, load_group, member_ids, special_chat_cooldown_target,
		special_chat_cooldown_until,
	},
	v0::social::is_blocked_either_way,
	v0::websocket::{send_to_owner, structs::ClientBoundPacket},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SpecialChatError {
	#[error(transparent)]
	Group(#[from] GroupError),
}

impl axum::response::IntoResponse for SpecialChatError {
	fn into_response(self) -> axum::response::Response {
		match self {
			Self::Group(error) => error.into_response(),
		}
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SpecialChatStatus {
	group_id: Option<i32>,
	cooldown_until: Option<DateTime<FixedOffset>>,
}

fn status_doc(op: TransformOperation) -> TransformOperation {
	op.id("getSpecialChatStatus")
		.summary("Get (or eagerly create) the player's Special Chat group")
		.description(
			"Every configured Special Chat recipient is a member, alongside \
			 the authenticated player, of a single ordinary group chat that \
			 this endpoint eagerly creates (with its opening message from a \
			 recipient, if one is configured) the first time it's called. \
			 From then on it behaves exactly like any other group chat — \
			 message it via the normal /groups endpoints.",
		)
		.tag("special-chat")
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/special-chat",
		ApiRouter::new().api_route("/", get_with(self::status, self::status_doc)),
	)
}

async fn get_or_create_special_chat_group(
	state: &ApiState,
	player: &entities::user::Model,
) -> Result<Option<(bool, entities::groups::Model, Vec<entities::user::Model>)>, SpecialChatError>
{
	use entities::{
		group_members, groups,
		prelude::*,
		sea_orm_active_enums::{GroupKind, GroupMemberRole},
	};

	let mut targets = Vec::new();
	for target_uuid in &state.special_chat_targets {
		if *target_uuid == player.minecraft_uuid {
			continue;
		}
		let Ok(target) = find_user_by_uuid(state, *target_uuid).await else {
			continue;
		};
		if is_blocked_either_way(state, player.id, target.id)
			.await
			.map_err(GroupError::from)?
		{
			continue;
		}
		targets.push(target);
	}

	if targets.is_empty() {
		return Ok(None);
	}

	let existing_group_id = GroupMembers::find()
		.filter(group_members::Column::UserId.eq(player.id))
		.inner_join(Groups)
		.filter(groups::Column::Kind.eq(GroupKind::Group))
		.filter(groups::Column::OwnerId.is_null())
		.filter(groups::Column::Name.is_null())
		.all(&state.database)
		.await
		.map_err(GroupError::from)?
		.into_iter()
		.map(|member| member.group_id)
		.next();

	if let Some(group_id) = existing_group_id {
		let existing_member_ids = member_ids(state, group_id).await.map_err(GroupError::from)?;
		for target in &targets {
			if !existing_member_ids.contains(&target.id) {
				GroupMembers::insert(group_members::ActiveModel {
					group_id: Set(group_id),
					user_id: Set(target.id),
					role: Set(GroupMemberRole::Member),
					joined_at: ActiveValue::NotSet,
				})
				.exec_without_returning(&state.database)
				.await
				.map_err(GroupError::from)?;
			}
		}
		return Ok(Some((false, load_group(state, group_id).await?, targets)));
	}

	let group = Groups::insert(groups::ActiveModel {
		kind: Set(GroupKind::Group),
		name: Set(None),
		owner_id: Set(None),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await
	.map_err(GroupError::from)?;

	for member_id in std::iter::once(player.id).chain(targets.iter().map(|target| target.id)) {
		GroupMembers::insert(group_members::ActiveModel {
			group_id: Set(group.id),
			user_id: Set(member_id),
			role: Set(GroupMemberRole::Member),
			joined_at: ActiveValue::NotSet,
		})
		.exec_without_returning(&state.database)
		.await
		.map_err(GroupError::from)?;
	}

	Ok(Some((true, group, targets)))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn status(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
) -> Result<Json<SpecialChatStatus>, SpecialChatError> {
	use entities::{group_messages, prelude::*};

	let Some((created, group, targets)) = get_or_create_special_chat_group(&state, &player).await?
	else {
		return Ok(Json(SpecialChatStatus { group_id: None, cooldown_until: None }));
	};

	if created && let Some(opening_message) = state.special_chat_auto_reply.as_deref()
		&& let Some(sender) = targets.first()
	{
		let message = GroupMessages::insert(group_messages::ActiveModel {
			group_id: Set(group.id),
			sender_id: Set(sender.id),
			content: Set(opening_message.to_string()),
			sent_at: ActiveValue::NotSet,
			edited_at: Set(None),
			deleted_at: Set(None),
			idempotency_key: Set(None),
			..Default::default()
		})
		.exec_with_returning(&state.database)
		.await
		.map_err(GroupError::from)?;

		send_to_owner(&state, player.minecraft_uuid, || ClientBoundPacket::GroupMessageReceived {
			group_id: group.id,
			message_id: message.id,
			sender: sender.minecraft_uuid,
			content: message.content.clone(),
		})
		.await;
	}

	let cooldown_until = match special_chat_cooldown_target(&state, &group, player.id)
		.await
		.map_err(GroupError::from)?
	{
		Some(cooldown_target_id) => special_chat_cooldown_until(&state, player.id, cooldown_target_id)
			.await
			.map_err(GroupError::from)?,
		None => None,
	};

	Ok(Json(SpecialChatStatus { group_id: Some(group.id), cooldown_until }))
}
