use std::collections::BTreeMap;

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
use entities::{player_mod, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, EntityTrait, QueryFilter as _, QuerySelect as _, Set, TransactionTrait,
};
use serde::Deserialize;

use crate::api::{
	ApiState,
	v0::account::{AuthenticatedPlayer, ClientKind},
};

const MAX_MODS: usize = 1000;
const MAX_FIELD_LEN: usize = 64;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ModsError {
	#[error("Unable to record mods: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for ModsError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(StatusCode::INTERNAL_SERVER_ERROR, self)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RequestBody {
	/// Mod ID to version for every top-level mod the client has loaded.
	#[schemars(example = &example_mods())]
	mods: BTreeMap<String, String>,
}

fn example_mods() -> BTreeMap<String, String> {
	BTreeMap::from([("sodium".to_owned(), "0.6.13+mc1.21.4".to_owned())])
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("putPlayerMods")
		.summary("Report the player's installed mods")
		.description(
			"Replaces the authorized player's reported mod list. Entries with an ID or \
			 version that does not look like one are dropped, as is anything past the \
			 first 1000.",
		)
		.tag("account")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/mods", put_with(self::endpoint, self::endpoint_doc))
}

fn valid_mod_id(id: &str) -> bool {
	id.len() <= MAX_FIELD_LEN
		&& id.starts_with(|c: char| c.is_ascii_lowercase())
		&& id
			.chars()
			.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

fn valid_version(version: &str) -> bool {
	!version.is_empty()
		&& version.len() <= MAX_FIELD_LEN
		&& version
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

#[tracing::instrument(level = "debug", skip(state, body))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticatedPlayer(player): AuthenticatedPlayer,
	client: ClientKind,
	Json(body): Json<RequestBody>,
) -> Result<NoContent, ModsError> {
	if client != ClientKind::Game {
		return Ok(NoContent);
	}

	let rows: Vec<_> = body
		.mods
		.into_iter()
		.filter(|(id, version)| valid_mod_id(id) && valid_version(version))
		.take(MAX_MODS)
		.map(|(mod_id, version)| player_mod::ActiveModel {
			player_id: Set(player.id),
			mod_id: Set(mod_id),
			version: Set(version),
			..Default::default()
		})
		.collect();

	let txn = state.database.begin().await?;
	User::find_by_id(player.id).lock_exclusive().one(&txn).await?;
	PlayerMod::delete_many()
		.filter(player_mod::Column::PlayerId.eq(player.id))
		.exec(&txn)
		.await?;
	if !rows.is_empty() {
		PlayerMod::insert_many(rows).exec_without_returning(&txn).await?;
	}
	txn.commit().await?;

	Ok(NoContent)
}

#[cfg(test)]
mod tests {
	use super::{valid_mod_id, valid_version};

	#[test]
	fn mod_ids_follow_fabric_rules() {
		assert!(valid_mod_id("sodium"));
		assert!(valid_mod_id("fabric-api_base2"));
		assert!(!valid_mod_id("Sodium"));
		assert!(!valid_mod_id("1mod"));
		assert!(!valid_mod_id(""));
		assert!(!valid_mod_id(&"a".repeat(65)));
	}

	#[test]
	fn versions_reject_junk() {
		assert!(valid_version("0.6.13+mc1.21.4"));
		assert!(valid_version("1.0.0-beta.2"));
		assert!(!valid_version(""));
		assert!(!valid_version("1.0 <script>"));
	}
}
