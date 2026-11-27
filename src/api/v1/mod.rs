mod analytics;
mod dashboard;
mod discounts;
mod events;
mod grants;
mod hello;
mod listings;
mod oneclient;
pub(super) mod party;
mod paynow;
mod player_cosmetics;
mod referrals;
mod roles;
mod settings;
mod special_chat;
mod store;
mod tokens;

pub(super) use oneclient::{MAX_UNCOMPRESSED_BYTES, SyncStorage, spawn_sync_sweeper};
pub(super) use paynow::spawn_expiry_sweeper;

use aide::axum::ApiRouter;

use crate::api::{
	ApiState,
	v0::{groups, social},
};

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.nest("/checkout", paynow::checkout_router().await)
		.nest("/paynow", paynow::webhook_router().await)
		.nest("/oneclient", oneclient::router())
		.nest("/social", social::v1_router())
		.nest("/groups", groups::v1_router())
		.merge(discounts::router())
		.merge(events::router())
		.merge(grants::router())
		.merge(listings::router())
		.merge(party::router())
		.merge(referrals::router())
		.merge(roles::router())
		.merge(settings::router())
		.merge(dashboard::router())
		.merge(special_chat::router())
		.merge(store::router())
		.merge(player_cosmetics::router())
		.merge(hello::router())
		.merge(tokens::router())
		.merge(analytics::setup_router().await)
}
