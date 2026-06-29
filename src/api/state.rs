use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
	sync::Arc,
	time::Duration,
};

use axum::extract::FromRef;
use entities::prelude::*;
use entities::sea_orm_active_enums::BodySlot;
use migrations::{Migrator, MigratorTrait};
use moka::future::Cache;
use pasetors::{
	keys::{Generate, SymmetricKey},
	version4::V4,
};
use reqwest::{Client, ClientBuilder};
use s3::{Bucket, creds::Credentials};
use sea_orm::{ConnectOptions, Database, DatabaseConnection, EntityTrait};
use tebex::{apis::plugin::TebexPluginApiClient, webhooks::axum::TebexWebhookState};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{api::cosmetics::CachedAssetInfo, commands::ServeArgs};

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
				endpoint: args.s3_bucket_endpoint.clone(),
			},
			Credentials::default().expect(
				"Unable to read s3 credentials (https://lib.rs/crates/aws-creds)",
			),
		)
		.expect("Unable to connect to s3 bucket")
		.with_path_style()
		.into();

		// Initialize asset cache with initial values
		let asset_cache = Cache::builder()
			.time_to_live(Duration::from_hours(2))
			.build();

		let assets = Asset::find()
			.all(&database)
			.await
			.expect("Unable to fetch assets from db");
		for asset in assets {
			let Ok(info) =
				CachedAssetInfo::from_db_model(&asset, s3_bucket.clone()).await
			else {
				warn!(
					"Unable to fetch cached asset info for asset id {}",
					asset.id
				);
				continue;
			};
			asset_cache.insert(asset.id, info).await;
		}

		let (equipment_persist_tx, equipment_persist_rx) =
			tokio::sync::mpsc::channel(256);
		tokio::spawn(persist_equipment_queue(
			database.clone(),
			equipment_persist_rx,
		));

		let (particle_color_persist_tx, particle_color_persist_rx) =
			tokio::sync::mpsc::channel(256);
		tokio::spawn(persist_particle_color_queue(
			database.clone(),
			particle_color_persist_rx,
		));

		// Return final state
		ApiState {
			tebex: TebexApiState {
				webhook_secret: Box::leak(
					args.tebex_webhook_secret.clone().into_boxed_str(),
				),
				plugin_client: TebexPluginApiClient::new(&args.tebex_game_server_secret)
					.expect("Unable to construct Tebex plugin API client"),
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
			asset_cache,
			realtime: RealtimeState::default(),
			equipment_persist_tx,
			particle_color_persist_tx,
			admin_password: args.admin_password.clone(),
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
	pub(super) asset_cache: Cache<i32, CachedAssetInfo>,
	pub(super) realtime: RealtimeState,
	pub(super) equipment_persist_tx: tokio::sync::mpsc::Sender<EquipmentPersistence>,
	pub(super) particle_color_persist_tx:
		tokio::sync::mpsc::Sender<ParticleColorPersistence>,
	pub(super) admin_password: String,
}

#[derive(Debug, Clone)]
pub(super) struct TebexApiState {
	webhook_secret: &'static str,
	pub(super) plugin_client: TebexPluginApiClient,
}

#[derive(Debug, Clone, Default)]
pub(super) struct RealtimeState {
	pub(super) connections:
		Arc<tokio::sync::RwLock<HashMap<ConnectionId, RealtimeConnection>>>,
	pub(super) connections_by_owner:
		Arc<tokio::sync::RwLock<HashMap<Uuid, HashSet<ConnectionId>>>>,
	pub(super) player_runtime:
		Arc<tokio::sync::RwLock<HashMap<Uuid, PlayerRuntimeState>>>,
	pub(super) watchers: Arc<tokio::sync::RwLock<HashMap<Uuid, HashSet<ConnectionId>>>>,
}

pub(super) type ConnectionId = Uuid;

#[derive(Debug, Clone)]
pub(super) struct RealtimeConnection {
	pub(super) owner: Uuid,
	pub(super) tx: tokio::sync::mpsc::UnboundedSender<
		crate::api::websocket::structs::ClientBoundPacket,
	>,
	pub(super) subscriptions: HashSet<Uuid>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct PlayerRuntimeState {
	pub(super) equipped: HashMap<BodySlot, i32>,
	pub(super) active_emote: Option<i32>,
	pub(super) particle_color: Option<i32>,
}

#[derive(Debug, Clone)]
pub(super) struct EquipmentPersistence {
	pub(super) player: Uuid,
	pub(super) slot: BodySlot,
	pub(super) cosmetic_id: Option<i32>,
}

#[derive(Debug, Clone)]
pub(super) struct ParticleColorPersistence {
	pub(super) player: Uuid,
	pub(super) color: Option<i32>,
}

async fn persist_equipment_queue(
	database: DatabaseConnection,
	mut rx: tokio::sync::mpsc::Receiver<EquipmentPersistence>,
) {
	use entities::{player_equipped_cosmetic, prelude::*, user};
	use sea_orm::{
		ActiveValue, ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::OnConflict,
	};

	while let Some(update) = rx.recv().await {
		let result = async {
			let Some(player) = User::find()
				.filter(user::Column::MinecraftUuid.eq(update.player))
				.one(&database)
				.await?
			else {
				return Ok::<(), sea_orm::DbErr>(());
			};

			if let Some(cosmetic_id) = update.cosmetic_id {
				PlayerEquippedCosmetic::insert(player_equipped_cosmetic::ActiveModel {
					player_id: Set(player.id),
					slot: Set(update.slot),
					cosmetic_id: Set(cosmetic_id),
					updated_at: ActiveValue::NotSet,
				})
				.on_conflict(
					OnConflict::columns([
						player_equipped_cosmetic::Column::PlayerId,
						player_equipped_cosmetic::Column::Slot,
					])
					.update_column(player_equipped_cosmetic::Column::CosmeticId)
					.to_owned(),
				)
				.exec(&database)
				.await?;
			} else {
				PlayerEquippedCosmetic::delete_many()
					.filter(player_equipped_cosmetic::Column::PlayerId.eq(player.id))
					.filter(player_equipped_cosmetic::Column::Slot.eq(update.slot))
					.exec(&database)
					.await?;
			}

			Ok(())
		}
		.await;

		if let Err(error) = result {
			warn!("Unable to persist websocket equipment update: {error}");
		}
	}
}

async fn persist_particle_color_queue(
	database: DatabaseConnection,
	mut rx: tokio::sync::mpsc::Receiver<ParticleColorPersistence>,
) {
	use entities::{prelude::*, user};
	use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

	while let Some(update) = rx.recv().await {
		let result = async {
			let Some(player) = User::find()
				.filter(user::Column::MinecraftUuid.eq(update.player))
				.one(&database)
				.await?
			else {
				return Ok::<(), sea_orm::DbErr>(());
			};

			let mut player: user::ActiveModel = player.into();
			player.particle_color = Set(update.color);
			player.update(&database).await?;

			Ok(())
		}
		.await;

		if let Err(error) = result {
			warn!("Unable to persist websocket particle color update: {error}");
		}
	}
}

impl FromRef<ApiState> for TebexWebhookState {
	fn from_ref(input: &ApiState) -> Self {
		Self {
			secret: Cow::Borrowed(input.tebex.webhook_secret),
		}
	}
}
