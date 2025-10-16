use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, ser::SerializeStruct as _};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum WebsocketError {
	#[error("A fatal websocket connection error")]
	Fatal(#[from] axum::Error),
	#[error("Unable to query database: {0}")]
	DatabaseQuery(#[from] sea_orm::error::DbErr),
	#[error("Unable to serialize response: {0}")]
	Serialization(#[from] serde_json::Error)
}

impl Serialize for WebsocketError {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer
	{
		let mut state = serializer.serialize_struct("WebsocketError", 2)?;
		state.serialize_field("error_code", match self {
			Self::Fatal(_) => "fatal",
			Self::DatabaseQuery(_) | Self::Serialization(_) => "internal_server_error"
		})?;
		state.serialize_field("message", &self.to_string())?;
		state.end()
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase", tag = "type")]
pub enum ServerBoundPacket {
	/// Fetches active cosmetics for a list of players in bulk
	GetActiveCosmetics { players: Vec<Uuid> }
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "PascalCase", tag = "type")]
pub enum ClientBoundPacket {
	CosmeticsInfo {
		cosmetics: HashMap<Uuid, Vec<i32>>
	},
	Error {
		#[serde(flatten)]
		#[schemars(skip)] // TODO: ?
		error: WebsocketError
	}
}
