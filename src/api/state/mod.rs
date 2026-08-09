//! Shared application state handed to every request handler.

mod persistence;
mod realtime;

use std::{sync::Arc, time::Duration};

use entities::prelude::*;
use migrations::{Migrator, MigratorTrait};
use moka::future::Cache;
use pasetors::{
	keys::{Generate, SymmetricKey},
	version4::V4,
};
use reqwest::{Client, ClientBuilder};
use s3::{Bucket, creds::Credentials};
use sea_orm::{ConnectOptions, Database, DatabaseConnection, EntityTrait};
use stripe_client::Client as StripeClient;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub(in crate::api) use self::{
	persistence::{EquipmentPersistence, ParticleColorPersistence},
	realtime::{
		ConnectionId, PlayerRuntimeState, PlaytimeSession, RealtimeConnection,
		RealtimeState,
	},
};
use uuid::Uuid;

use crate::{
	api::v0::{
		cosmetics::CachedAssetInfo,
		oidc::{self, AuthorizationCode, OidcSigningKey},
	},
	commands::ServeArgs,
};

/// How long a rendered asset stays in the in-memory cache.
const ASSET_CACHE_TTL: Duration = Duration::from_hours(2);
/// How long to wait for a free database connection before giving up.
const DATABASE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a player must wait between global chat messages.
const GLOBAL_CHAT_COOLDOWN: Duration = Duration::from_secs(2);

const USER_AGENT: &str = "PolyPlus Backend";

#[derive(Debug, Clone)]
pub(super) struct ApiState {
	pub(super) stripe: StripeApiState,
	pub(super) database: DatabaseConnection,
	pub(super) client: Client,
	pub(super) render_client: Client,
	pub(super) paseto_key: SymmetricKey<V4>,
	pub(super) s3_bucket: Arc<Bucket>,
	pub(super) asset_cache: Cache<i32, CachedAssetInfo>,
	pub(super) realtime: RealtimeState,
	pub(super) equipment_persist_tx: mpsc::Sender<EquipmentPersistence>,
	pub(super) particle_color_persist_tx: mpsc::Sender<ParticleColorPersistence>,
	pub(super) admin_password: String,
	pub(super) render_service_url: String,
	pub(super) global_chat_cooldown: Cache<i32, ()>,
	pub(super) oidc_issuer: String,
	pub(super) oidc_signing_key: Arc<OidcSigningKey>,
	pub(super) oidc_codes: Cache<String, AuthorizationCode>,
	pub(super) special_chat_targets: Vec<Uuid>,
	pub(super) special_chat_auto_reply: Option<String>,
}

#[derive(Clone)]
pub(super) struct StripeApiState {
	pub(super) client: StripeClient,
	pub(super) webhook_secret: String,
	pub(super) success_url: String,
	pub(super) cancel_url: String,
}

// i love leaking secrets
impl std::fmt::Debug for StripeApiState {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("StripeApiState").finish_non_exhaustive()
	}
}

impl ApiState {
	#[tracing::instrument(skip_all, name = "initialize_state", level = "debug")]
	pub(super) async fn new(args: &ServeArgs) -> Self {
		let database = connect_database(&args.database_url).await;
		let s3_bucket = connect_s3_bucket(args);
		let asset_cache = build_asset_cache(&database, &s3_bucket).await;

		let realtime = RealtimeState::default();
		persistence::spawn_playtime_flush(database.clone(), realtime.playtime.clone());

		let oidc_signing_key = oidc::load_or_generate_signing_key(&database).await;

		ApiState {
			stripe: StripeApiState::new(args),
			client: build_http_client(true),
			render_client: build_http_client(false),
			paseto_key: SymmetricKey::generate()
				.expect("Unable to generate paseto signing key"),
			asset_cache,
			realtime,
			equipment_persist_tx: persistence::spawn_equipment_queue(database.clone()),
			particle_color_persist_tx: persistence::spawn_particle_color_queue(
				database.clone(),
			),
			admin_password: args.admin_password.clone(),
			render_service_url: args.render_service_url.clone(),
			global_chat_cooldown: Cache::builder()
				.time_to_live(GLOBAL_CHAT_COOLDOWN)
				.build(),
			oidc_issuer: args.oidc_issuer.clone(),
			oidc_signing_key: Arc::new(oidc_signing_key),
			oidc_codes: oidc::new_authorization_code_cache(),
			special_chat_targets: args.special_chat_targets.clone(),
			special_chat_auto_reply: args.special_chat_auto_reply.clone(),
			s3_bucket,
			database,
		}
	}
}

impl StripeApiState {
	fn new(args: &ServeArgs) -> Self {
		StripeApiState {
			client: StripeClient::new(args.stripe_secret.clone()),
			webhook_secret: args.stripe_webhook_secret.clone(),
			success_url: args.stripe_success_url.clone(),
			cancel_url: args.stripe_cancel_url.clone(),
		}
	}
}

/// Connects to the database and brings it up to the latest migration.
async fn connect_database(database_url: &str) -> DatabaseConnection {
	info!("Attempting to create database connection");
	let database = Database::connect({
		let mut opts = ConnectOptions::new(database_url);

		opts.acquire_timeout(DATABASE_ACQUIRE_TIMEOUT);
		opts.sqlx_logging(false);

		opts
	})
	.await
	.expect("Unable to connect to database");

	info!("Database connected, applying migrations");
	Migrator::up(&database, None)
		.await
		.expect("Failure migrating database");
	info!("Database successfully initialized");

	database
}

fn connect_s3_bucket(args: &ServeArgs) -> Arc<Bucket> {
	Bucket::new(
		&args.s3_bucket_name,
		s3::Region::Custom {
			region: args.s3_bucket_region.clone(),
			endpoint: args.s3_bucket_endpoint.clone(),
		},
		Credentials::default()
			.expect("Unable to read s3 credentials (https://lib.rs/crates/aws-creds)"),
	)
	.expect("Unable to connect to s3 bucket")
	.with_path_style()
	.into()
}

fn build_http_client(https_only: bool) -> Client {
	ClientBuilder::new()
		.https_only(https_only)
		.user_agent(USER_AGENT)
		.build()
		.expect("Unable to build reqwest client")
}

/// Builds the asset cache, warming it with every asset currently in the database.
async fn build_asset_cache(
	database: &DatabaseConnection,
	s3_bucket: &Arc<Bucket>,
) -> Cache<i32, CachedAssetInfo> {
	let asset_cache = Cache::builder().time_to_live(ASSET_CACHE_TTL).build();

	let assets = Asset::find()
		.all(database)
		.await
		.expect("Unable to fetch assets from db");

	for asset in assets {
		let Ok(info) = CachedAssetInfo::from_db_model(&asset, s3_bucket.clone()).await
		else {
			warn!(
				"Unable to fetch cached asset info for asset id {}",
				asset.id
			);
			continue;
		};

		asset_cache.insert(asset.id, info).await;
	}

	asset_cache
}
