use aide::{
	OperationIo,
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Query, State},
	http::StatusCode,
	response::IntoResponse,
};
use chrono::{DateTime, FixedOffset};
use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait, EntityTrait, Order, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};

use crate::api::ApiState;

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum SearchError {
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for SearchError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

/// The order in which results are returned.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
	/// Oldest first (by creation time).
	Oldest,
	/// Newest first (by creation time).
	#[default]
	Newest,
	/// Cheapest first (by base price).
	Ascending,
	/// Most expensive first (by base price).
	Descending,
}

/// The maximum number of results allowed per page.
const MAX_NB: u64 = 100;

fn default_nb() -> u64 {
	50
}

fn default_page() -> u64 {
	1
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchQuery {
	/// The number of results per page, capped at 100.
	#[serde(default = "default_nb")]
	nb: u64,
	/// The 1-indexed page to return.
	#[serde(default = "default_page")]
	page: u64,
	/// The order results are returned in.
	#[serde(default)]
	sort: Sort,
	/// A substring to match against the name.
	text: Option<String>,
	/// Restrict results to one or more cosmetic types (including `emote`),
	/// comma-separated (e.g. `cape,emote`). Omit to return every type.
	#[serde(default, deserialize_with = "deserialize_types")]
	types: Option<Vec<CosmeticType>>,
}

/// Parses a comma-separated list of cosmetic types (e.g. `cape,emote`),
/// deferring to each type's own deserialization. Empty segments are ignored,
/// and an empty list is treated as no filter.
fn deserialize_types<'de, D>(de: D) -> Result<Option<Vec<CosmeticType>>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	use serde::de::{Error, IntoDeserializer};

	let Some(raw) = Option::<String>::deserialize(de)? else {
		return Ok(None);
	};

	let types = raw
		.split(',')
		.map(str::trim)
		.filter(|part| !part.is_empty())
		.map(|part| CosmeticType::deserialize(part.into_deserializer()))
		.collect::<Result<Vec<_>, serde::de::value::Error>>()
		.map_err(Error::custom)?;

	Ok((!types.is_empty()).then_some(types))
}

/// A single enabled cosmetic or emote in the search results.
#[derive(Debug, Serialize, JsonSchema)]
struct CosmeticSearchInfo {
	id: i32,
	name: String,
	description: Option<String>,
	collection: Option<i32>,
	r#type: CosmeticType,
	base_price: Option<f32>,
	discount_rate: Option<i32>,
	asset_id: Option<i32>,
	created_at: DateTime<FixedOffset>,
}

impl CosmeticSearchInfo {
	fn from_cosmetic(cosmetic: entities::cosmetic::Model) -> Self {
		CosmeticSearchInfo {
			id: cosmetic.id,
			name: cosmetic
				.name
				.unwrap_or_else(|| format!("Cosmetic {}", cosmetic.id)),
			description: cosmetic.description,
			collection: cosmetic.collection,
			r#type: cosmetic.r#type,
			base_price: cosmetic.base_price,
			discount_rate: cosmetic.discount_rate,
			asset_id: cosmetic.asset_id,
			created_at: cosmetic.created_at,
		}
	}
}

/// Pagination metadata describing the returned page within the full result set.
#[derive(Debug, Default, Serialize, JsonSchema)]
struct Pagination {
	/// The 1-indexed page these results are from.
	page: u64,
	/// The number of results on this page.
	count: u64,
	/// The total number of results matching the query across all pages.
	total_items: u64,
	/// The total number of pages available for the query.
	total_pages: u64,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct SearchResponse {
	results: Vec<CosmeticSearchInfo>,
	pagination: Pagination,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("searchCosmetics")
		.summary("Search cosmetics and emotes")
		.description(
			"Lists enabled cosmetics and emotes, paginated by `nb` per page and \
			 1-indexed `page`, optionally filtered by a `text` substring of the name \
			 and a `type`.",
		)
		.tag("cosmetics")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/cosmetics/search",
		get_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, SearchError> {
	use entities::{cosmetic, prelude::*};

	// Return results in the range [nb * (page - 1); nb * page).
	let nb = query.nb.min(MAX_NB);
	let offset = nb.saturating_mul(query.page.saturating_sub(1));

	let (column, order) = match query.sort {
		Sort::Oldest => (cosmetic::Column::CreatedAt, Order::Asc),
		Sort::Newest => (cosmetic::Column::CreatedAt, Order::Desc),
		Sort::Ascending => (cosmetic::Column::BasePrice, Order::Asc),
		Sort::Descending => (cosmetic::Column::BasePrice, Order::Desc),
	};

	// Emotes are cosmetics with type `emote`, so a single cosmetic query covers
	// every type. A `type` filter (including `emote`) narrows the results.
	let mut find = Cosmetic::find()
		.filter(cosmetic::Column::Enabled.eq(true))
		.filter(cosmetic::Column::BasePrice.is_not_null());
	if let Some(text) = &query.text {
		find = find.filter(cosmetic::Column::Name.contains(text.as_str()));
	}
	if let Some(kinds) = &query.types {
		find = find.filter(cosmetic::Column::Type.is_in(kinds.clone()));
	}

	// Count all matches before paginating so the response can report totals.
	let total_items = find.clone().count(&state.database).await?;

	let results: Vec<CosmeticSearchInfo> = find
		.order_by(column, order)
		.order_by(cosmetic::Column::Id, Order::Asc)
		.offset(offset)
		.limit(nb)
		.all(&state.database)
		.await?
		.into_iter()
		.map(CosmeticSearchInfo::from_cosmetic)
		.collect();

	let pagination = Pagination {
		page: query.page,
		count: results.len() as u64,
		total_items,
		total_pages: if nb == 0 { 0 } else { total_items.div_ceil(nb) },
	};

	Ok(Json(SearchResponse {
		results,
		pagination,
	}))
}
