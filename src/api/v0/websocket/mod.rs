use aide::axum::ApiRouter;

use crate::api::ApiState;

mod endpoint;
pub mod structs;

pub(crate) use endpoint::{broadcast_all, send_to_owner, unequip_revoked};

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().merge(endpoint::router())
}
