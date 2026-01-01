use std::{borrow::Cow, collections::HashMap};

use schemars::{JsonSchema, json_schema};
use serde::{Deserialize, Serialize, ser::SerializeStruct as _};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum WebsocketError {
	#[error("A fatal websocket connection error")]
	Fatal(#[from] axum::Error),
	#[error("Unable to query database: {0}")]
	DatabaseQuery(#[from] sea_orm::error::DbErr),
	#[error("Unable to serialize response: {0}")]
	Serialization(#[from] serde_json::Error),
}

impl WebsocketError {
	const ERROR_CODES: &[&str] = &["fatal", "internal_server_error"];

	pub fn error_code(&self) -> &'static str {
		match self {
			Self::Fatal(_) => Self::ERROR_CODES[0],
			Self::DatabaseQuery(_) | Self::Serialization(_) => Self::ERROR_CODES[1],
		}
	}
}

impl Serialize for WebsocketError {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut state = serializer.serialize_struct("WebsocketError", 2)?;
		state.serialize_field("error_code", self.error_code())?;
		state.serialize_field("message", &self.to_string())?;
		state.end()
	}
}

impl JsonSchema for WebsocketError {
	fn schema_name() -> Cow<'static, str> {
		Cow::Borrowed("WebsocketError")
	}

	fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
		json_schema!({
			"type": "object",
			"properties": {
				"error_code": {
					"enum": WebsocketError::ERROR_CODES,
					"description": "The machine-readable unique error code",
					"example": "internal_server_error"
				},
				"message": {
					"type": "string",
					"description": "The human-readable error message"
				}
			}
		})
	}
}

/// A JSON object that a client can send in the websocket connection
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase", tag = "type")]
pub enum ServerBoundPacket {
	/// Fetches active cosmetics for a list of players in bulk
	GetActiveCosmetics {
		/// An array of player UUIDs to include in the bulk lookup
		players: Vec<Uuid>,
	},
}

/// A JSON object that the server will send to the client in the websocket
/// connection
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "PascalCase", tag = "type")]
pub enum ClientBoundPacket {
	/// Information on player UUIDs and what cosmetics they own, sent in
	/// response to [ServerBoundPacket::GetActiveCosmetics]
	CosmeticsInfo {
		/// An object mapping player UUIDs to a list of their active cosmetic
		/// IDs. These cosmetic IDs should be resolved seperately, as no other
		/// information is given in this response.
		///
		/// Any players without active cosmetics are not returned in this
		/// object.
		#[schemars(example = HashMap::from([("424ef6d0-4774-4f8c-8bef-8f62ebdac9c0", [1,2])]))]
		cosmetics: HashMap<Uuid, Vec<i32>>,
	},
	/// An error response from the server
	Error {
		#[serde(flatten)]
		error: WebsocketError,
	},
}
