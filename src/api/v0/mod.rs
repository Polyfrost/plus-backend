pub(super) mod account;
mod assets;
mod bundles;
mod category;
mod collections;
pub(super) mod cosmetics;
// mod global_chat; // todo: Currently disabled
mod groups;
mod links;
pub(super) mod oidc;
mod players;
pub(super) mod sessions;
mod social;
mod special_chat;
mod stripe;
mod tags;
mod transactions;
pub(super) mod websocket;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest("/stripe", stripe::setup_router().await)
		.nest("/account", account::setup_router().await)
		.nest("/transactions", transactions::setup_router().await)
		.merge(assets::setup_router().await)
		.merge(bundles::setup_router().await)
		.merge(collections::setup_router().await)
		.merge(links::setup_router().await)
		.merge(oidc::setup_router().await)
		.merge(players::setup_router().await)
		.merge(cosmetics::setup_router().await)
		// .merge(global_chat::setup_router().await)
		.merge(groups::setup_router().await)
		.merge(tags::setup_router().await)
		.merge(category::setup_router().await)
		.merge(sessions::setup_router().await)
		.merge(social::setup_router().await)
		.merge(special_chat::setup_router().await)
		.merge(websocket::setup_router().await)
}
