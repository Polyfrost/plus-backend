use aide::transform::TransformOperation;
use axum::{Json, extract::State};

use super::PrivateAnalyticsAuth;
use crate::api::{ApiState, state::ConnectionCounts};

pub(super) fn realtime_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsRealtime")
		.summary("Get live websocket connection counts")
		.description(
			"Websocket connections held open right now, split into `game` (the token \
			 was issued to a client that reported mod and platform details at login) \
			 and `other`.\n\nThis is counted in memory per process, so it starts from \
			 zero on restart and only covers the instance that answered the request.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn realtime_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
) -> Json<ConnectionCounts> {
	Json(state.realtime.connection_counts().await)
}
