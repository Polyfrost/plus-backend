use std::{borrow::Cow, time::Duration};

use axum::extract::FromRef;
use migrations::{Migrator, MigratorTrait};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use tebex::{apis::plugin::TebexPluginApiClient, webhooks::axum::TebexWebhookState};
use tracing::info;

use crate::commands::ServeArgs;

impl ApiState {
	#[tracing::instrument(skip_all, name = "initialize_state", level = "debug")]
	pub(super) async fn new(args: &ServeArgs) -> Self {
		// Setup database
		info!("Attempting to create database connection");
		let database = Database::connect({
			let mut opts = ConnectOptions::new(&args.database_url);

			opts.acquire_timeout(Duration::new(3, 0)); // Shorten connection timeout
			opts.sqlx_logging(false); // SeaORM has its own logging, disable SQLx's

			opts
		})
		.await
		.expect("Unable to connect to database");

		info!("Database connected, applying migrations");
		Migrator::up(&database, None)
			.await
			.expect("Failure migrating database");
		info!("Database successfully initialized");

		// Return final state
		ApiState {
			tebex: TebexApiState {
				webhook_secret: Box::leak(
					args.tebex_webhook_secret.clone().into_boxed_str()
				),
				plugin_client: TebexPluginApiClient::new(&args.tebex_game_server_secret)
					.expect("Unable to construct Tebex plugin API client")
			},
			database
		}
	}
}

#[derive(Debug, Clone)]
pub(super) struct ApiState {
	pub(super) tebex: TebexApiState,
	pub(super) database: DatabaseConnection
}

#[derive(Debug, Clone)]
pub(super) struct TebexApiState {
	webhook_secret: &'static str,
	pub(super) plugin_client: TebexPluginApiClient
}

impl FromRef<ApiState> for TebexWebhookState {
	fn from_ref(input: &ApiState) -> Self {
		Self {
			secret: Cow::Borrowed(input.tebex.webhook_secret)
		}
	}
}
