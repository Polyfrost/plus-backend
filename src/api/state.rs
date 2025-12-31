use std::{borrow::Cow, sync::Arc, time::Duration};

use axum::extract::FromRef;
use entities::prelude::*;
use migrations::{Migrator, MigratorTrait};
use moka::future::Cache;
use pasetors::{
	keys::{Generate, SymmetricKey},
	version4::V4
};
use reqwest::{Client, ClientBuilder};
use s3::{Bucket, creds::Credentials};
use sea_orm::{ConnectOptions, Database, DatabaseConnection, EntityTrait};
use tebex::{apis::plugin::TebexPluginApiClient, webhooks::axum::TebexWebhookState};
use tracing::{info, warn};

use crate::{api::cosmetics::CachedCosmeticInfo, commands::ServeArgs};

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

		// Setup s3 bucket
		let s3_bucket: Arc<Bucket> = Bucket::new(
			&args.s3_bucket_name,
			s3::Region::Custom {
				region: args.s3_bucket_region.clone(),
				endpoint: args.s3_bucket_endpoint.clone()
			},
			Credentials::default().expect(
				"Unable to read s3 credentials (https://lib.rs/crates/aws-creds)"
			)
		)
		.expect("Unable to connect to s3 bucket")
		.with_path_style()
		.into();

		// Initialize cosmetic cache with initial values
		let cosmetic_cache = Cache::builder()
			.time_to_live(Duration::from_hours(2))
			.build();

		let cosmetics = Cosmetic::find()
			.all(&database)
			.await
			.expect("Unable to fetch cosmetics from db");
		for cosmetic in cosmetics {
			let Ok(info) =
				CachedCosmeticInfo::from_db_model(&cosmetic, s3_bucket.clone()).await
			else {
				warn!(
					"Unable to fetch cached cosmetic info for cosmetic id {}",
					cosmetic.id
				);
				continue;
			};
			cosmetic_cache.insert(cosmetic.id, info).await;
		}

		// Return final state
		ApiState {
			tebex: TebexApiState {
				webhook_secret: Box::leak(
					args.tebex_webhook_secret.clone().into_boxed_str()
				),
				plugin_client: TebexPluginApiClient::new(&args.tebex_game_server_secret)
					.expect("Unable to construct Tebex plugin API client")
			},
			database,
			client: ClientBuilder::new()
				.https_only(true)
				.user_agent("PolyPlus Backend")
				.build()
				.expect("Unable to build reqwest HTTPS client"),
			paseto_key: SymmetricKey::generate()
				.expect("Unable to generate paseto signing key"),
			s3_bucket,
			cosmetic_cache,
			msa: MsaAuthState {
				client_id: args.msa_client_id.clone(),
				client_secret: args.msa_client_secret.clone(),
				redirect_uri: args.msa_redirect_uri.clone(),
				pkce_cache: Cache::builder()
					.time_to_live(Duration::from_mins(10))
					.build()
			}
		}
	}
}

#[derive(Debug, Clone)]
pub(super) struct ApiState {
	pub(super) tebex: TebexApiState,
	pub(super) database: DatabaseConnection,
	pub(super) client: Client,
	pub(super) paseto_key: SymmetricKey<V4>,
	pub(super) s3_bucket: Arc<Bucket>,
	pub(super) cosmetic_cache: Cache<i32, CachedCosmeticInfo>,
	pub(super) msa: MsaAuthState
}

#[derive(Debug, Clone)]
pub(super) struct TebexApiState {
	webhook_secret: &'static str,
	pub(super) plugin_client: TebexPluginApiClient
}

#[derive(Debug, Clone)]
pub(super) struct MsaAuthState {
	pub(super) client_id: String,
	pub(super) client_secret: String,
	pub(super) redirect_uri: String,
	pub(super) pkce_cache: Cache<String, String>
}

impl FromRef<ApiState> for TebexWebhookState {
	fn from_ref(input: &ApiState) -> Self {
		Self {
			secret: Cow::Borrowed(input.tebex.webhook_secret)
		}
	}
}
