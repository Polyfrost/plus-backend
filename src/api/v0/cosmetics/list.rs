use std::{cmp::Ordering, collections::HashMap};

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
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryTrait};
use serde::{Deserialize, Serialize};

use crate::api::{
	ApiState,
	v0::cosmetics::{
		CosmeticInfo, group_cosmetics, in_enabled_group, is_redundant_variant, load_groups,
	},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("Unable to fetch user data from database: {0}")]
	DatabaseFetch(#[from] sea_orm::error::DbErr),
	#[error("Unable to presign S3 URLs: {0}")]
	S3Presign(#[from] s3::error::S3Error),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("listCosmetics")
		.summary("List all cosmetics")
		.description("Lists all cosmetics, including their URLs and unique IDs")
		.tag("cosmetics")
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to fetch cosmetics",
				)
			},
		)
}

impl IntoResponse for ResponseError {
	fn into_response(self) -> axum::response::Response {
		crate::api::error_response(
			match self {
				ResponseError::S3Presign(_) => StatusCode::INTERNAL_SERVER_ERROR,
				ResponseError::DatabaseFetch(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

/// Information about the player's cosmetics
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Response {
	cosmetics: Vec<CosmeticInfo>,
}

/// The direction results are returned in.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
	Asc,
	#[default]
	Desc,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
	CreatedAt,
	#[default]
	Popularity,
	Price,
	Name,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListFilters {
	r#type: Option<CosmeticType>,
	#[serde(default)]
	sort_by: SortBy,
	#[serde(default)]
	sort: SortOrder,
}

/// Sort inputs for one entry of the response, aggregated over every variant
/// that entry collapses.
#[derive(Debug)]
struct SortKey {
	created_at_min: DateTime<FixedOffset>,
	created_at_max: DateTime<FixedOffset>,
	price_min: Option<f32>,
	price_max: Option<f32>,
	purchases: i64,
}

impl SortKey {
	fn add(&mut self, cosmetic: &entities::cosmetic::Model) {
		self.created_at_min = self.created_at_min.min(cosmetic.created_at);
		self.created_at_max = self.created_at_max.max(cosmetic.created_at);
		if let Some(price) = cosmetic.base_price {
			self.price_min = Some(self.price_min.map_or(price, |min| min.min(price)));
			self.price_max = Some(self.price_max.map_or(price, |max| max.max(price)));
		}
		self.purchases += i64::from(cosmetic.purchase_count);
	}

	fn from_cosmetic(cosmetic: &entities::cosmetic::Model) -> Self {
		Self {
			created_at_min: cosmetic.created_at,
			created_at_max: cosmetic.created_at,
			price_min: cosmetic.base_price,
			price_max: cosmetic.base_price,
			purchases: i64::from(cosmetic.purchase_count),
		}
	}
}

/// Compares two optional values, keeping `None` last regardless of `order`.
fn cmp_missing_last<T>(
	a: Option<T>,
	b: Option<T>,
	order: SortOrder,
	cmp: impl Fn(&T, &T) -> Ordering,
) -> Ordering {
	match (a, b) {
		(Some(a), Some(b)) => directed(cmp(&a, &b), order),
		(Some(_), None) => Ordering::Less,
		(None, Some(_)) => Ordering::Greater,
		(None, None) => Ordering::Equal,
	}
}

fn directed(ordering: Ordering, order: SortOrder) -> Ordering {
	match order {
		SortOrder::Asc => ordering,
		SortOrder::Desc => ordering.reverse(),
	}
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/cosmetics", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	Query(filters): Query<ListFilters>,
) -> Result<Json<Response>, ResponseError> {
	let mut response = Response::default();

	{
		use entities::{cosmetic, prelude::*};

		let cosmetics = Cosmetic::find()
			.filter(cosmetic::Column::Enabled.eq(true))
			.filter(in_enabled_group())
			.apply_if(filters.r#type, |query, v| {
				query.filter(cosmetic::Column::Type.eq(v))
			})
			.find_with_related(CosmeticAllowedSlot)
			.all(&state.database)
			.await?;

		let mut rows = Vec::with_capacity(cosmetics.len());
		for (cosmetic, allowed) in cosmetics {
			let asset = match cosmetic.asset_id {
				Some(asset_id) => {
					Asset::find_by_id(asset_id).one(&state.database).await?
				}
				None => None,
			};
			let cover_asset = match cosmetic.cover_asset_id {
				Some(asset_id) => {
					Asset::find_by_id(asset_id).one(&state.database).await?
				}
				None => None,
			};
			let allowed_slots = allowed.into_iter().map(|s| s.slot).collect();
			rows.push((cosmetic, asset, cover_asset, allowed_slots));
		}

		let groups = load_groups(&state.database).await?;

		let mut keys: HashMap<i32, SortKey> = HashMap::new();
		for (cosmetic, ..) in &rows {
			if is_redundant_variant(cosmetic.variant_name.as_deref()) {
				continue;
			}
			let bucket_id = cosmetic
				.group_id
				.filter(|id| groups.contains_key(id))
				.unwrap_or(cosmetic.id);
			keys.entry(bucket_id)
				.and_modify(|key| key.add(cosmetic))
				.or_insert_with(|| SortKey::from_cosmetic(cosmetic));
		}

		let mut cosmetics = group_cosmetics(
			rows,
			groups,
			state.asset_cache.clone(),
			state.s3_bucket.clone(),
			true,
		)
		.await?;

		let (sort_by, order) = (filters.sort_by, filters.sort);
		cosmetics.sort_by(|a, b| {
			if sort_by == SortBy::Name {
				return directed(
					a.name.to_lowercase().cmp(&b.name.to_lowercase()),
					order,
				);
			}

			let (Some(a), Some(b)) = (keys.get(&a.id), keys.get(&b.id)) else {
				return Ordering::Equal;
			};

			match (sort_by, order) {
				(SortBy::Name, _) => unreachable!("handled above"),
				(SortBy::CreatedAt, SortOrder::Asc) => {
					a.created_at_min.cmp(&b.created_at_min)
				}
				(SortBy::CreatedAt, SortOrder::Desc) => {
					b.created_at_max.cmp(&a.created_at_max)
				}
				(SortBy::Popularity, _) => directed(a.purchases.cmp(&b.purchases), order),
				(SortBy::Price, SortOrder::Asc) => {
					cmp_missing_last(a.price_min, b.price_min, order, |a, b| {
						a.total_cmp(b)
					})
				}
				(SortBy::Price, SortOrder::Desc) => {
					cmp_missing_last(a.price_max, b.price_max, order, |a, b| {
						a.total_cmp(b)
					})
				}
			}
		});

		response.cosmetics = cosmetics;
	};

	Ok(Json(response))
}
