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
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use sea_orm::{ActiveValue, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	account::AuthenticatedPlayer,
	groups::{GroupError, find_user_by_uuid, get_or_create_dm_group},
	social::is_blocked_either_way,
	websocket::{send_to_owner, structs::ClientBoundPacket},
};

const MAX_MESSAGE_LENGTH: usize = 4000;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SpecialChatError {
	#[error("That player does not have a Special Chat")]
	NotASpecialChatTarget,
	#[error("You cannot message yourself")]
	SelfTarget,
	#[error("That player has blocked you")]
	Blocked,
	#[error("Message content must not be empty and must be under {MAX_MESSAGE_LENGTH} characters")]
	InvalidMessage,
	#[error(
		"You may only send one message to a given Special Chat recipient every \
		 72 hours"
	)]
	RateLimited,
	#[error(transparent)]
	Group(#[from] GroupError),
}

impl IntoResponse for SpecialChatError {
	fn into_response(self) -> axum::response::Response {
		match self {
			Self::NotASpecialChatTarget => {
				(StatusCode::NOT_FOUND, self.to_string()).into_response()
			}
			Self::SelfTarget | Self::Blocked => {
				(StatusCode::CONFLICT, self.to_string()).into_response()
			}
			Self::InvalidMessage => (StatusCode::BAD_REQUEST, self.to_string()).into_response(),
			Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, self.to_string()).into_response(),
			Self::Group(error) => error.into_response(),
		}
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SpecialChatTarget {
	player: Uuid,
	group_id: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendRequest {
	content: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SentMessage {
	group_id: i32,
	message_id: i64,
	sent_at: DateTime<FixedOffset>,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listSpecialChatTargets")
		.summary("List Special Chat recipients")
		.description(
			"Lists the Special Chat recipients every player's Messaging \
			 interface auto-instantiates a chat session for. group_id is \
			 present once a chat has actually been opened (lazily, on first \
			 message).",
		)
		.tag("special-chat")
}

fn send_doc(op: TransformOperation) -> TransformOperation {
	op.id("sendSpecialChatMessage")
		.summary("Send a message to a Special Chat recipient")
		.description(
			"Sends a message to a configured Special Chat recipient without \
			 requiring prior authorization or mutual acceptance via the \
			 Friends System. Subject to a strictly enforced 72-hour cooldown \
			 between consecutive messages from the same sender to the same \
			 recipient. The recipient may still block a specific sender.",
		)
		.tag("special-chat")
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/special-chat",
		ApiRouter::new()
			.api_route("/", get_with(self::list, self::list_doc))
			.api_route("/{player}/messages", post_with(self::send, self::send_doc)),
	)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	_player: AuthenticatedPlayer,
) -> Result<Json<Vec<SpecialChatTarget>>, SpecialChatError> {
	use entities::{group_members, groups, prelude::*};
	use sea_orm::{ColumnTrait, QueryFilter};

	let mut targets = Vec::with_capacity(state.special_chat_targets.len());
	for player in &state.special_chat_targets {
		let group_id = match find_user_by_uuid(&state, *player).await {
			Ok(user) => GroupMembers::find()
				.filter(group_members::Column::UserId.eq(user.id))
				.inner_join(Groups)
				.filter(groups::Column::Kind.eq(entities::sea_orm_active_enums::GroupKind::Dm))
				.one(&state.database)
				.await
				.map_err(GroupError::from)?
				.map(|member| member.group_id),
			Err(_) => None,
		};

		targets.push(SpecialChatTarget {
			player: *player,
			group_id,
		});
	}

	Ok(Json(targets))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn send(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Path(target): Path<Uuid>,
	Json(body): Json<SendRequest>,
) -> Result<(StatusCode, Json<SentMessage>), SpecialChatError> {
	use entities::{group_messages, prelude::*};

	if !state.special_chat_targets.contains(&target) {
		return Err(SpecialChatError::NotASpecialChatTarget);
	}
	if target == player.minecraft_uuid {
		return Err(SpecialChatError::SelfTarget);
	}

	let content = body.content.trim();
	if content.is_empty() || content.len() > MAX_MESSAGE_LENGTH {
		return Err(SpecialChatError::InvalidMessage);
	}

	let target = find_user_by_uuid(&state, target).await?;

	if is_blocked_either_way(&state, player.id, target.id)
		.await
		.map_err(GroupError::from)?
	{
		return Err(SpecialChatError::Blocked);
	}

	let cooldown_key = (player.id, target.id);
	if state.special_chat_cooldown.get(&cooldown_key).await.is_some() {
		return Err(SpecialChatError::RateLimited);
	}

	let (_, group) = get_or_create_dm_group(&state, player.id, target.id).await?;

	let message = GroupMessages::insert(group_messages::ActiveModel {
		group_id: Set(group.id),
		sender_id: Set(player.id),
		content: Set(content.to_string()),
		sent_at: ActiveValue::NotSet,
		edited_at: Set(None),
		deleted_at: Set(None),
		idempotency_key: Set(None),
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await
	.map_err(GroupError::from)?;

	state.special_chat_cooldown.insert(cooldown_key, ()).await;

	send_to_owner(&state, target.minecraft_uuid, || {
		ClientBoundPacket::GroupMessageReceived {
			group_id: group.id,
			message_id: message.id,
			sender: player.minecraft_uuid,
			content: message.content.clone(),
		}
	})
	.await;

	Ok((
		StatusCode::CREATED,
		Json(SentMessage {
			group_id: group.id,
			message_id: message.id,
			sent_at: message.sent_at,
		}),
	))
}
