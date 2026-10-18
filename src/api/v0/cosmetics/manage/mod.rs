mod create;
mod delete;
mod render_cover;
mod replace_asset;
mod update;
mod update_variant;

use aide::axum::ApiRouter;

use crate::api::ApiState;

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/manage",
		ApiRouter::new()
			.merge(create::router())
			.merge(update::router())
			.merge(update_variant::router())
			.merge(replace_asset::router())
			.merge(delete::router())
			.merge(render_cover::router()),
	)
}
