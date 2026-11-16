//! Lets administrators change server settings without a restart.

use aide::axum::{ApiRouter, routing::get_with};
use axum::{Json, extract::State};
use serde_json::Value;

use crate::api::{
	ApiState,
	permissions::{Require, perm},
	settings::{Settings, SettingsError},
};

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/admin/settings",
		get_with(self::get, |op| {
			op.id("getServerSettings")
				.summary("Get the server settings")
				.tag("settings")
		})
		.patch_with(self::update, |op| {
			op.id("updateServerSettings")
				.summary("Change server settings")
				.description(
					"Takes a JSON merge patch of the settings, so only the fields sent \
					 change, and returns the settings now in force.",
				)
				.tag("settings")
		}),
	)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn get(
	State(state): State<ApiState>,
	_auth: Require<{ perm::ADMINISTRATOR }>,
) -> Json<Settings> {
	Json(Settings::clone(&state.settings.get()))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn update(
	State(state): State<ApiState>,
	Require(staff, _): Require<{ perm::ADMINISTRATOR }>,
	Json(patch): Json<Value>,
) -> Result<Json<Settings>, SettingsError> {
	let settings = state.settings.update(patch)?;
	tracing::info!(by = %staff, ?settings, "Changed server settings");
	Ok(Json(Settings::clone(&settings)))
}
