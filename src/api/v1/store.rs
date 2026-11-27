//! Read-only admin views of the store: the whole catalogue, disabled and
//! unpriced entries included, and every order with who placed it.

use std::collections::{HashMap, HashSet};

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation
};
use axum::{
	Json,
	extract::{Query, State},
	http::StatusCode,
	response::IntoResponse
};
use entities::{
	asset,
	asset_color_slot,
	cosmetic_allowed_slot,
	prelude::*,
	sea_orm_active_enums::*,
	transaction,
	transaction_line,
	user
};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait,
	EntityTrait,
	sea_query::NullOrdering,
	PaginatorTrait,
	QueryFilter,
	QueryOrder
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		permissions::{Require, RequireAny, perm},
		v0::{
			cosmetics::CachedAssetInfo,
			players::lookup::resolve_username,
			websocket::colors::allowed_slots
		}
	},
	utils::pagination::{MAX_PAGE_SIZE, default_page, default_page_size}
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum StoreError {
	#[error("No player by that name or id")]
	UnknownPlayer,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr)
}

impl IntoResponse for StoreError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::UnknownPlayer => StatusCode::NOT_FOUND,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR
			},
			self
		)
	}
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/admin/catalog", get_with(self::catalog, self::catalog_doc))
		.api_route("/admin/orders", get_with(self::orders, self::orders_doc))
}

fn catalog_doc(op: TransformOperation) -> TransformOperation {
	op.id("adminCatalog")
		.summary("List the whole catalogue")
		.description(
			"Every cosmetic, variant group and bundle, including disabled ones and \
			 cosmetics that were uploaded but never made into a product (no \
			 `store_product_id`). Needs any catalogue permission: `MANAGE_STORE`, `MANAGE_COSMETICS`, `GRANT_COSMETICS`, `MANAGE_DISCOUNTS` or `MANAGE_REFERRALS`."
		)
		.tag("store")
}

fn orders_doc(op: TransformOperation) -> TransformOperation {
	op.id("adminOrders")
		.summary("List orders")
		.description(
			"Every transaction, newest first unless `sort` says otherwise, with the \
			 player it was for, the player who paid when that was someone else, and \
			 each line. `player` narrows it to one player's orders, as buyer or \
			 recipient, by UUID or username; `provider`, `status`, `min_amount` / \
			 `max_amount` (minor units, orders without an amount drop out) and \
			 `from` / `to` (RFC 3339) narrow it further. Requires `VIEW_ORDERS`."
		)
		.tag("store")
}

#[derive(Debug, Serialize, JsonSchema)]
struct CatalogResponse {
	cosmetics: Vec<CatalogCosmetic>,
	groups: Vec<CatalogGroup>,
	bundles: Vec<CatalogBundle>
}

#[derive(Debug, Serialize, JsonSchema)]
struct CatalogCosmetic {
	id: i32,
	name: Option<String>,
	r#type: CosmeticType,
	enabled: bool,
	group_id: Option<i32>,
	variant_name: Option<String>,
	model_variant: Option<String>,
	variant_order: i32,
	/// Null for an upload that is not sold on its own.
	store_product_id: Option<String>,
	base_price: Option<f32>,
	collection: Option<i32>,
	description: Option<String>,
	purchase_count: i32,
	url: Option<String>,
	cover_url: Option<String>,
	/// Looping animation for cosmetics that move.
	preview_url: Option<String>,
	tag_ids: Vec<i32>,
	created_at: String,
	variant_key: Option<String>,
	asset_id: Option<i32>,
	slots: Vec<BodySlot>,
	color_slots: Vec<CatalogColorSlot>
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct CatalogColorSlot {
	name: String
}

fn color_slots(kind: &CosmeticType, declared: Vec<String>) -> Vec<CatalogColorSlot> {
	allowed_slots(kind, declared)
		.into_iter()
		.map(|name| CatalogColorSlot { name })
		.collect()
}

#[derive(Debug, Serialize, JsonSchema)]
struct CatalogGroup {
	id: i32,
	name: String,
	r#type: CosmeticType,
	enabled: bool
}

#[derive(Debug, Serialize, JsonSchema)]
struct CatalogBundle {
	id: i32,
	name: String,
	description: Option<String>,
	enabled: bool,
	collection: Option<i32>,
	store_product_id: Option<String>,
	base_price: Option<f32>,
	cover_url: Option<String>,
	cosmetic_ids: Vec<i32>,
	created_at: String
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn catalog(
	State(state): State<ApiState>,
	_auth: RequireAny<{ perm::CATALOG_READERS }>,
) -> Result<Json<CatalogResponse>, StoreError> {
	let db = &state.database;
	let cosmetics = Cosmetic::find().all(db).await?;
	let groups = CosmeticGroup::find().all(db).await?;
	let bundles = Bundles::find().all(db).await?;

	let asset_ids = cosmetics
		.iter()
		.flat_map(|c| [c.asset_id, c.cover_asset_id, c.preview_asset_id])
		.chain(bundles.iter().map(|b| b.asset_id))
		.flatten()
		.collect::<HashSet<_>>();
	let assets: HashMap<i32, asset::Model> = Asset::find()
		.filter(asset::Column::Id.is_in(asset_ids))
		.all(db)
		.await?
		.into_iter()
		.map(|a| (a.id, a))
		.collect();
	let url = |id: Option<i32>| {
		CachedAssetInfo::asset_url(
			id.and_then(|id| assets.get(&id)),
			&state.s3_public_url
		)
	};

	let mut tags = group(
		TagsCosmetic::find()
			.all(db)
			.await?
			.into_iter()
			.map(|row| (row.cosmetic_id, row.tag_id))
	);
	let mut contents = group(
		BundlesCosmetics::find()
			.all(db)
			.await?
			.into_iter()
			.map(|row| (row.bundle_id, row.cosmetic_id))
	);
	let mut slots = group(
		CosmeticAllowedSlot::find()
			.order_by_asc(cosmetic_allowed_slot::Column::Slot)
			.all(db)
			.await?
			.into_iter()
			.map(|row| (row.cosmetic_id, row.slot))
	);
	let cosmetic_assets = cosmetics
		.iter()
		.filter_map(|c| c.asset_id)
		.collect::<HashSet<_>>();
	let declared = group(
		AssetColorSlot::find()
			.filter(asset_color_slot::Column::AssetId.is_in(cosmetic_assets))
			.all(db)
			.await?
			.into_iter()
			.map(|row| (row.asset_id, row.slot))
	);

	Ok(Json(CatalogResponse {
		cosmetics: cosmetics
			.into_iter()
			.map(|c| CatalogCosmetic {
				url: url(c.asset_id),
				cover_url: url(c.cover_asset_id),
				preview_url: url(c.preview_asset_id),
				tag_ids: tags.remove(&c.id).unwrap_or_default(),
				slots: slots.remove(&c.id).unwrap_or_default(),
				color_slots: color_slots(
					&c.r#type,
					c.asset_id
						.and_then(|id| declared.get(&id).cloned())
						.unwrap_or_default()
				),
				variant_key: c.variant_key,
				asset_id: c.asset_id,
				id: c.id,
				name: c.name,
				r#type: c.r#type,
				enabled: c.enabled,
				group_id: c.group_id,
				variant_name: c.variant_name,
				model_variant: c.model_variant,
				variant_order: c.variant_order,
				store_product_id: c.store_product_id,
				base_price: c.base_price,
				collection: c.collection,
				description: c.description,
				purchase_count: c.purchase_count,
				created_at: c.created_at.to_rfc3339()
			})
			.collect(),
		groups: groups
			.into_iter()
			.map(|g| CatalogGroup {
				id: g.id,
				name: g.name,
				r#type: g.r#type,
				enabled: g.enabled
			})
			.collect(),
		bundles: bundles
			.into_iter()
			.map(|b| CatalogBundle {
				cover_url: url(b.asset_id),
				cosmetic_ids: contents.remove(&b.id).unwrap_or_default(),
				id: b.id,
				name: b.name,
				description: b.description,
				enabled: b.enabled,
				collection: b.collection,
				store_product_id: b.store_product_id,
				base_price: b.base_price,
				created_at: b.created_at.to_rfc3339()
			})
			.collect()
	}))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OrdersQuery {
	#[serde(default = "default_page")]
	page: u64,
	#[serde(default = "default_page_size")]
	nb: u64,
	/// A UUID or username.
	#[serde(default)]
	player: Option<String>,
	#[serde(default)]
	provider: Option<TransactionProvider>,
	#[serde(default)]
	status: Option<TransactionStatus>,
	/// Charged at least this much, in minor units.
	#[serde(default)]
	min_amount: Option<i64>,
	/// Charged at most this much, in minor units.
	#[serde(default)]
	max_amount: Option<i64>,
	/// Placed at or after this instant.
	#[serde(default)]
	from: Option<chrono::DateTime<chrono::Utc>>,
	/// Placed before this instant.
	#[serde(default)]
	to: Option<chrono::DateTime<chrono::Utc>>,
	#[serde(default)]
	sort: OrderSort
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OrderSort {
	#[default]
	Newest,
	Oldest,
	AmountDesc,
	AmountAsc
}

#[derive(Debug, Serialize, JsonSchema)]
struct OrdersResponse {
	orders: Vec<Order>,
	total_items: u64,
	total_pages: u64
}

#[derive(Debug, Serialize, JsonSchema)]
struct Player {
	uuid: Uuid,
	username: Option<String>
}

#[derive(Debug, Serialize, JsonSchema)]
struct Order {
	id: i32,
	provider: TransactionProvider,
	provider_transaction_id: Option<String>,
	status: TransactionStatus,
	/// Who the order was for.
	player: Option<Player>,
	/// Who paid, when that was someone else: a gift.
	buyer: Option<Player>,
	amount_minor: Option<i64>,
	discount_minor: Option<i64>,
	currency: Option<String>,
	refunded_minor: i64,
	refunded_at: Option<String>,
	charged_back_at: Option<String>,
	created_at: String,
	lines: Vec<OrderLine>
}

#[derive(Debug, Serialize, JsonSchema)]
struct OrderLine {
	product_id: String,
	bundle_id: Option<i32>,
	cosmetic_group_id: Option<i32>,
	cosmetic_id: Option<i32>,
	recipient: Option<Player>,
	quantity: i32,
	price_minor: i64,
	discount_minor: i64,
	total_minor: i64,
	currency: String,
	status: TransactionStatus,
	returned_minor: i64
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn orders(
	State(state): State<ApiState>,
	_auth: Require<{ perm::VIEW_ORDERS }>,
	Query(query): Query<OrdersQuery>
) -> Result<Json<OrdersResponse>, StoreError> {
	let db = &state.database;
	let mut select = match query.sort {
		OrderSort::Newest => Transaction::find().order_by_desc(transaction::Column::Id),
		OrderSort::Oldest => Transaction::find().order_by_asc(transaction::Column::Id),
		OrderSort::AmountDesc => Transaction::find()
			.order_by_with_nulls(
				transaction::Column::AmountMinor,
				sea_orm::Order::Desc,
				NullOrdering::Last
			)
			.order_by_desc(transaction::Column::Id),
		OrderSort::AmountAsc => Transaction::find()
			.order_by_with_nulls(
				transaction::Column::AmountMinor,
				sea_orm::Order::Asc,
				NullOrdering::Last
			)
			.order_by_desc(transaction::Column::Id)
	};
	if let Some(provider) = query.provider {
		select = select.filter(transaction::Column::Provider.eq(provider));
	}
	if let Some(status) = query.status {
		select = select.filter(transaction::Column::Status.eq(status));
	}
	if let Some(min) = query.min_amount {
		select = select.filter(transaction::Column::AmountMinor.gte(min));
	}
	if let Some(max) = query.max_amount {
		select = select.filter(transaction::Column::AmountMinor.lte(max));
	}
	if let Some(from) = query.from {
		select = select.filter(transaction::Column::CreatedAt.gte(from));
	}
	if let Some(to) = query.to {
		select = select.filter(transaction::Column::CreatedAt.lt(to));
	}

	if let Some(player) = query
		.player
		.as_deref()
		.map(str::trim)
		.filter(|p| !p.is_empty())
	{
		let uuid = match Uuid::parse_str(player) {
			Ok(uuid) => uuid,
			Err(_) =>
				resolve_username(&state, player)
					.await
					.map_err(|_| StoreError::UnknownPlayer)?
					.id,
		};
		let user = User::find()
			.filter(user::Column::MinecraftUuid.eq(uuid))
			.one(db)
			.await?
			.ok_or(StoreError::UnknownPlayer)?;
		select = select.filter(
			transaction::Column::PlayerId
				.eq(user.id)
				.or(transaction::Column::Buyer.eq(user.id))
		);
	}

	let paginator = select.paginate(db, query.nb.clamp(1, MAX_PAGE_SIZE));
	let totals = paginator.num_items_and_pages().await?;
	let rows = paginator.fetch_page(query.page.max(1) - 1).await?;

	let mut lines = group(
		TransactionLine::find()
			.filter(transaction_line::Column::TransactionId.is_in(rows.iter().map(|t| t.id)))
			.order_by_asc(transaction_line::Column::Id)
			.all(db)
			.await?
			.into_iter()
			.map(|line| (line.transaction_id, line))
	);

	let user_ids = rows
		.iter()
		.flat_map(|t| [Some(t.player_id), t.buyer])
		.chain(lines.values().flatten().map(|l| l.recipient_id))
		.flatten()
		.collect::<HashSet<_>>();
	let players: HashMap<i32, user::Model> = User::find()
		.filter(user::Column::Id.is_in(user_ids))
		.all(db)
		.await?
		.into_iter()
		.map(|u| (u.id, u))
		.collect();
	let player = |id: Option<i32>| {
		id.and_then(|id| players.get(&id)).map(|u| Player {
			uuid: u.minecraft_uuid,
			username: u.username.clone()
		})
	};

	let orders = rows
		.into_iter()
		.map(|t| Order {
			player: player(Some(t.player_id)),
			// A buyer who is also the recipient is not a gift.
			buyer: player(t.buyer.filter(|&b| b != t.player_id)),
			lines: lines
				.remove(&t.id)
				.unwrap_or_default()
				.into_iter()
				.map(|l| OrderLine {
					recipient: player(l.recipient_id),
					product_id: l.product_id,
					bundle_id: l.bundle_id,
					cosmetic_group_id: l.cosmetic_group_id,
					cosmetic_id: l.cosmetic_id,
					quantity: l.quantity,
					price_minor: l.price_minor,
					discount_minor: l.discount_minor,
					total_minor: l.total_minor,
					currency: l.currency,
					status: l.status,
					returned_minor: l.returned_minor
				})
				.collect(),
			id: t.id,
			provider: t.provider,
			provider_transaction_id: t.provider_transaction_id,
			status: t.status,
			amount_minor: t.amount_minor,
			discount_minor: t.discount_minor,
			currency: t.currency,
			refunded_minor: t.refunded_minor,
			refunded_at: t.refunded_at.map(|at| at.to_rfc3339()),
			charged_back_at: t.charged_back_at.map(|at| at.to_rfc3339()),
			created_at: t.created_at.to_rfc3339()
		})
		.collect();

	Ok(Json(OrdersResponse {
		orders,
		total_items: totals.number_of_items,
		total_pages: totals.number_of_pages
	}))
}

fn group<V>(pairs: impl Iterator<Item = (i32, V)>) -> HashMap<i32, Vec<V>> {
	let mut map: HashMap<i32, Vec<V>> = HashMap::new();
	for (key, value) in pairs {
		map.entry(key).or_default().push(value);
	}
	map
}

#[cfg(test)]
mod tests {
	use entities::sea_orm_active_enums::{BodySlot, CosmeticType};
	use serde_json::json;

	use super::{CatalogCosmetic, color_slots};

	#[test]
	fn catalogue_rows_list_their_variant_asset_and_sorted_colour_slots() {
		let names = |kind, declared: &[&str]| {
			let declared = declared.iter().map(|name| (*name).to_owned()).collect();
			color_slots(&kind, declared).into_iter().map(|slot| slot.name).collect::<Vec<_>>()
		};
		assert_eq!(names(CosmeticType::Wings, &["trim", "glow"]), ["glow", "trim"]);
		assert!(names(CosmeticType::Cape, &[]).is_empty());
		assert_eq!(names(CosmeticType::Aura, &[]), ["aura"]);
		assert_eq!(names(CosmeticType::Aura, &["aura"]), ["aura"]);

		let row = CatalogCosmetic {
			id: 5,
			name: Some("Pride".to_owned()),
			r#type: CosmeticType::Nameplate,
			enabled: false,
			group_id: Some(2),
			variant_name: Some("Rainbow".to_owned()),
			model_variant: None,
			variant_order: 1,
			store_product_id: None,
			base_price: None,
			collection: None,
			description: None,
			purchase_count: 0,
			url: None,
			cover_url: None,
			preview_url: None,
			tag_ids: Vec::new(),
			created_at: "2026-11-25T12:00:00+00:00".to_owned(),
			variant_key: Some("rainbow".to_owned()),
			asset_id: Some(40),
			slots: vec![BodySlot::Nameplate],
			color_slots: color_slots(&CosmeticType::Nameplate, vec!["ink".to_owned()])
		};
		let body = serde_json::to_value(row).unwrap();
		assert_eq!((&body["variant_key"], &body["asset_id"], &body["enabled"]), (&json!("rainbow"), &json!(40), &json!(false)));
		assert_eq!((&body["slots"], &body["color_slots"]), (&json!(["nameplate"]), &json!([{ "name": "ink" }])));
	}
}
