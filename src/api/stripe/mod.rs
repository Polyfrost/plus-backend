mod create;
mod webhook;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.route("/create", axum::routing::post(create::endpoint))
}
