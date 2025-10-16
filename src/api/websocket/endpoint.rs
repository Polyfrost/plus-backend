use std::collections::HashMap;

use aide::{axum::{routing::ApiMethodDocs, ApiRouter}, openapi::Operation, transform::TransformOperation};
use axum::{
	body::Body,
	extract::{State, WebSocketUpgrade, ws::Message},
	routing::get
};
use http::{Response, StatusCode};
use sea_orm::{ColumnTrait as _, RelationTrait as _, EntityTrait as _, JoinType, QueryFilter, QuerySelect};

use crate::api::{
	cosmetics::ActiveCosmetics, websocket::structs::{ClientBoundPacket, ServerBoundPacket, WebsocketError}, ApiState
};

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.route("/websocket", get(self::endpoint))
		.api_route_docs("/websocket", ApiMethodDocs::new("get", {
			let mut operation = Operation::default();

			_ = TransformOperation::new(&mut operation)
				.id("websocket")
				.summary("Open a websocket connection to the server")
				.description("Establishes a websocket connection to the server.")
				.tag("misc")
				.response_with::<{ StatusCode::SWITCHING_PROTOCOLS.as_u16() }, (), _>(|res| {
					res.description(
						"Communication will continue over the WebSocket protocol"
					)
				});

			operation
		}))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(State(state): State<ApiState>, ws: WebSocketUpgrade) -> Response<Body> {
	ws.on_upgrade(async move |mut socket| {
		while let Some(msg) = socket.recv().await {
			let result: Result<(), WebsocketError> = try {
				let msg = msg?;

				let parsed =
					serde_json::from_slice::<ServerBoundPacket>(&msg.into_data())?;

				match parsed {
					ServerBoundPacket::GetActiveCosmetics { players } => {
						use entities::{prelude::*, user, user_cosmetic, cosmetic};

						let response = User::find()
							.find_with_related(UserCosmetic)
							.join(JoinType::LeftJoin, cosmetic::Relation::UserCosmetic.def().rev())
							.filter(user::Column::MinecraftUuid.is_in(players))
							.filter(user_cosmetic::Column::Active.eq(true))
							.filter(cosmetic::Column::Type.is_in(ActiveCosmetics::NAMES))
							.all(&state.database)
							.await?
							.into_iter()
							.fold(HashMap::new(), |mut acc, (user, cosmetics)| {
								for cosmetic in cosmetics {
									acc.entry(user.minecraft_uuid)
										.or_insert(Vec::new())
										.push(cosmetic.cosmetic);
								}
								acc
							});

						let serialized =
							serde_json::to_string(&ClientBoundPacket::CosmeticsInfo {
								cosmetics: response
							})
							.expect("CosmeticsInfo serialization should never fail");

						socket.send(Message::Text(serialized.into())).await?;
					}
				}
			};

			match result {
				Ok(_) => continue,
				Err(WebsocketError::Fatal(_)) => return,
				Err(e) => {
					let e = ClientBoundPacket::Error { error: e };
					let serialized = serde_json::to_string(&e).expect(
						"Serializing the error struct should never fail"
					);
					let Ok(_) = socket.send(Message::Text(serialized.into())).await
					else {
						return;
					};
				}
			}
		}
	})
}
