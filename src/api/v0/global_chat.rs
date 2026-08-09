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
	extract::{Query, State},
	http::StatusCode,
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{
	ApiState,
	v0::account::AuthenticatedPlayer,
	v0::websocket::{broadcast_all, structs::ClientBoundPacket},
};

const MAX_MESSAGE_LENGTH: usize = 512;
const DEFAULT_PAGE_SIZE: u64 = 100;
const MAX_PAGE_SIZE: u64 = 200;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum GlobalChatError {
	#[error("Message content must not be empty and must be under {MAX_MESSAGE_LENGTH} characters")]
	InvalidMessage,
	#[error("You are sending messages too quickly, please slow down")]
	RateLimited,
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for GlobalChatError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::InvalidMessage => StatusCode::BAD_REQUEST,
				Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GlobalChatMessage {
	id: i64,
	sender: Uuid,
	content: String,
	sent_at: DateTime<FixedOffset>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendRequest {
	content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HistoryQuery {
	before: Option<i64>,
	limit: Option<u64>,
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listGlobalChatMessages")
		.summary("List recent Global Chat history")
		.tag("global-chat")
}

fn send_doc(op: TransformOperation) -> TransformOperation {
	op.id("sendGlobalChatMessage")
		.summary("Send a Global Chat message")
		.description(
			"Posts a message to the centralized, cross-server public chat \
			 channel. Subject to a short per-player cooldown.",
		)
		.tag("global-chat")
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/chat/global",
		ApiRouter::new()
			.api_route("/", get_with(self::list, self::list_doc))
			.api_route("/", post_with(self::send, self::send_doc)),
	)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	_player: AuthenticatedPlayer,
	Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<GlobalChatMessage>>, GlobalChatError> {
	use entities::{global_chat_messages, prelude::*, user};

	let limit = query.limit.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE);

	let mut select = GlobalChatMessages::find();
	if let Some(before) = query.before {
		select = select.filter(global_chat_messages::Column::Id.lt(before));
	}

	let rows = select
		.order_by_desc(global_chat_messages::Column::Id)
		.limit(limit)
		.all(&state.database)
		.await?;

	let sender_ids = rows.iter().map(|row| row.sender_id).collect::<Vec<_>>();
	let senders = User::find()
		.filter(user::Column::Id.is_in(sender_ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| (user.id, user.minecraft_uuid))
		.collect::<std::collections::HashMap<_, _>>();

	Ok(Json(
		rows.into_iter()
			.filter_map(|row| {
				senders.get(&row.sender_id).map(|uuid| GlobalChatMessage {
					id: row.id,
					sender: *uuid,
					content: row.content,
					sent_at: row.sent_at,
				})
			})
			.collect(),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn send(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	Json(body): Json<SendRequest>,
) -> Result<(StatusCode, Json<GlobalChatMessage>), GlobalChatError> {
	use entities::{global_chat_messages, prelude::*};

	let content = body.content.trim();
	if content.is_empty() || content.len() > MAX_MESSAGE_LENGTH {
		return Err(GlobalChatError::InvalidMessage);
	}

	if state.global_chat_cooldown.get(&player.id).await.is_some() {
		return Err(GlobalChatError::RateLimited);
	}
	state.global_chat_cooldown.insert(player.id, ()).await;

	let message = GlobalChatMessages::insert(global_chat_messages::ActiveModel {
		sender_id: Set(player.id),
		content: Set(content.to_string()),
		sent_at: ActiveValue::NotSet,
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	broadcast_all(&state, || ClientBoundPacket::GlobalChatMessageReceived {
		message_id: message.id,
		sender: player.minecraft_uuid,
		content: message.content.clone(),
	})
	.await;

	Ok((
		StatusCode::CREATED,
		Json(GlobalChatMessage {
			id: message.id,
			sender: player.minecraft_uuid,
			content: message.content,
			sent_at: message.sent_at,
		}),
	))
}
