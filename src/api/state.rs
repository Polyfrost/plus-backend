use axum::extract::FromRef;
use tebex::webhooks::axum::TebexWebhookState;
use tokio::fs::File;

use crate::commands::{ServeArgs, TebexWebhookSecret};

impl ApiState {
	pub(super) async fn new(args: &ServeArgs) -> Self {
		ApiState {
			tebex: TebexApiState {
				// TODO: write a bpaf function parser for this insead (fn generates OptionParser with normal and "-file" variants)
				webhook_secret: match &args.tebex_webhook_secret {
					TebexWebhookSecret::File(path) => File::open(path).await.expect("Unable to read file containing"),
					TebexWebhookSecret::Raw(_) => todo!()
				}
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
