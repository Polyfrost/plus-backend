pub(super) mod account;
mod analytics;
mod assets;
mod bundles;
mod category;
mod collections;
pub(super) mod cosmetics;
mod links;
mod players;
mod stripe;
mod tags;
mod transactions;
pub(super) mod websocket;

use aide::axum::ApiRouter;

use crate::api::ApiState;

/// The original, unversioned API. It is served from the root of the server
/// rather than from a `/v0` prefix, so that clients written before the
/// versioning scheme existed keep working unchanged.
pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest("/stripe", stripe::setup_router().await)
		.nest("/account", account::setup_router().await)
		.nest("/transactions", transactions::setup_router().await)
		.merge(assets::setup_router().await)
		.merge(bundles::setup_router().await)
		.merge(collections::setup_router().await)
		.merge(links::setup_router().await)
		.merge(analytics::setup_router().await)
		.merge(players::setup_router().await)
		.merge(cosmetics::setup_router().await)
		.merge(tags::setup_router().await)
		.merge(category::setup_router().await)
		.merge(websocket::setup_router().await)
}
