use axum::extract::FromRef;
use tebex::webhooks::axum::TebexWebhookState;

use crate::commands::ServeArgs;

impl ApiState {
	pub(super) async fn new(args: &ServeArgs) -> Self {
		ApiState {
			tebex: TebexApiState {
				webhook_secret: args.tebex_webhook_secret.clone()
			}
		}
	}
}

#[derive(Debug, Clone)]
pub(super) struct ApiState {
	tebex: TebexApiState
}

#[derive(Debug, Clone)]
pub(super) struct TebexApiState {
	webhook_secret: String
}

impl FromRef<ApiState> for TebexWebhookState {
	fn from_ref(input: &ApiState) -> Self {
		Self {
			secret: input.tebex.webhook_secret.clone()
		}
	}
}
