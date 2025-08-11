use std::time::Duration;

use axum::extract::FromRef;
use migrations::{Migrator, MigratorTrait};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use tebex::webhooks::axum::TebexWebhookState;

use crate::commands::ServeArgs;

impl ApiState {
	pub(super) async fn new(args: &ServeArgs) -> Self {
		// Setup database
		let database = Database::connect({
			let mut opts = ConnectOptions::new(&args.database_url);

			opts.acquire_timeout(Duration::new(3, 0)); // Shorten connection timeout
			opts.sqlx_logging(false); // SeaORM has its own logging, disable SQLx's

			opts
		})
		.await
		.expect("Unable to connect to database");

		Migrator::up(&database, None)
			.await
			.expect("Failure migrating database");

		// Return final state
		ApiState {
			tebex: TebexApiState {
				webhook_secret: args.tebex_webhook_secret.clone()
			},
			database
		}
	}
}

#[derive(Debug, Clone)]
pub(super) struct ApiState {
	tebex: TebexApiState,
	pub(super) database: DatabaseConnection
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
