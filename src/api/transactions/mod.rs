mod player;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().route("/player", axum::routing::get(player::endpoint))
}
