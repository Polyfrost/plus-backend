mod analytics;
mod dashboard;
mod discounts;
mod grants;
mod hello;
mod listings;
mod oneclient;
pub(super) mod party;
mod paynow;
mod referrals;
mod roles;
mod special_chat;
mod store;
mod tokens;

pub(super) use oneclient::{SyncStorage, spawn_sync_sweeper};
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
		.merge(grants::router())
		.merge(listings::router())
		.merge(party::router())
		.merge(referrals::router())
		.merge(roles::router())
		.merge(dashboard::router())
		.merge(special_chat::router())
		.merge(store::router())
		.merge(hello::router())
		.merge(tokens::router())
		.merge(analytics::setup_router().await)
}
