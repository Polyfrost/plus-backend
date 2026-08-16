use aide::transform::TransformOperation;
use axum::{
	Json,
	extract::{Query, State},
};
use std::collections::HashMap;

use chrono::{NaiveDate, Utc};
use entities::{analytics_cosmetic_daily, analytics_cosmetic_snapshot, prelude::*};
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, EntityTrait, FromQueryResult, QueryFilter as _, QueryOrder as _,
	QuerySelect as _,
	sea_query::{Alias, Expr, SimpleExpr},
};
use serde::{Deserialize, Serialize};

use super::{
	AnalyticsError, AnalyticsPeriod, PrivateAnalyticsAuth, resolve_series_bounds,
};
use crate::{api::ApiState, utils::pagination::MAX_PAGE_SIZE};

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum CatalogSort {
	#[default]
	Views,
	Acquisitions,
	Conversion,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct CatalogQuery {
	#[serde(flatten)]
	period: AnalyticsPeriod,
	#[serde(default)]
	sort: CatalogSort,
	#[serde(default = "default_limit")]
	limit: u64,
}

fn default_limit() -> u64 {
	50
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CatalogResponse {
	start: NaiveDate,
	end: NaiveDate,
	cosmetics: Vec<CatalogEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CatalogEntry {
	cosmetic_id: i32,
	name: Option<String>,
	views: i64,
	acquisitions: i64,
	acquisitions_paid: i64,
	acquisitions_granted: i64,
	/// `acquisitions / views`, absent when the cosmetic was never viewed.
	conversion: Option<f64>,
	/// From the most recent weekly snapshot at or before `end`.
	owners: Option<i32>,
	equipped: Option<i32>,
	/// `equipped / owners`: how many owners actually wear it.
	shelf_rate: Option<f64>,
}

#[derive(Debug, FromQueryResult)]
struct CatalogRow {
	cosmetic_id: i32,
	views: Option<i64>,
	acquisitions: Option<i64>,
	paid: Option<i64>,
	granted: Option<i64>,
}

pub(super) fn catalog_doc(op: TransformOperation) -> TransformOperation {
	op.id("getAnalyticsCatalog")
		.summary("Get per-cosmetic performance")
		.description(
			"Views and acquisitions per cosmetic over the period.\n\nViews are counted \
			 on `/cosmetics/view/{id}` only, so a cosmetic seen in a list but never \
			 opened does not count. Conversion is acquisitions divided by views and can \
			 exceed 1 for cosmetics acquired in a bundle without being opened first.",
		)
		.tag("analytics")
}

#[tracing::instrument(level = "debug", skip(state))]
pub(super) async fn catalog_endpoint(
	State(state): State<ApiState>,
	_auth: PrivateAnalyticsAuth,
	Query(query): Query<CatalogQuery>,
) -> Result<Json<CatalogResponse>, AnalyticsError> {
	let (start, end) =
		resolve_series_bounds(query.period.validate()?, Utc::now().date_naive())?;
	let limit = query.limit.clamp(1, MAX_PAGE_SIZE);

	let views = Expr::col(analytics_cosmetic_daily::Column::Views)
		.sum()
		.cast_as(Alias::new("bigint"));
	let acquisitions = Expr::col(analytics_cosmetic_daily::Column::Acquisitions)
		.sum()
		.cast_as(Alias::new("bigint"));

	let conversion: SimpleExpr = Expr::case(
		Expr::expr(views.clone()).gt(0),
		Expr::expr(acquisitions.clone())
			.cast_as(Alias::new("double precision"))
			.div(views.clone()),
	)
	.finally(0)
	.into();

	let order = match query.sort {
		CatalogSort::Views => views.clone(),
		CatalogSort::Acquisitions => acquisitions.clone(),
		CatalogSort::Conversion => conversion,
	};

	let rows = AnalyticsCosmeticDaily::find()
		.filter(analytics_cosmetic_daily::Column::Day.gte(start))
		.filter(analytics_cosmetic_daily::Column::Day.lte(end))
		.select_only()
		.column(analytics_cosmetic_daily::Column::CosmeticId)
		.column_as(views, "views")
		.column_as(acquisitions, "acquisitions")
		.column_as(
			Expr::col(analytics_cosmetic_daily::Column::AcquisitionsPaid)
				.sum()
				.cast_as(Alias::new("bigint")),
			"paid",
		)
		.column_as(
			Expr::col(analytics_cosmetic_daily::Column::AcquisitionsGranted)
				.sum()
				.cast_as(Alias::new("bigint")),
			"granted",
		)
		.group_by(analytics_cosmetic_daily::Column::CosmeticId)
		.order_by_desc(order)
		.limit(limit)
		.into_model::<CatalogRow>()
		.all(&state.database)
		.await?;

	let names: HashMap<i32, Option<String>> = Cosmetic::find()
		.filter(
			entities::cosmetic::Column::Id.is_in(rows.iter().map(|row| row.cosmetic_id)),
		)
		.all(&state.database)
		.await?
		.into_iter()
		.map(|found| (found.id, found.name))
		.collect();

	let snapshot_week = AnalyticsCosmeticSnapshot::find()
		.filter(analytics_cosmetic_snapshot::Column::WeekStart.lte(end))
		.order_by_desc(analytics_cosmetic_snapshot::Column::WeekStart)
		.one(&state.database)
		.await?
		.map(|row| row.week_start);

	let mut stock: HashMap<i32, (i32, i32)> = HashMap::new();
	if let Some(week) = snapshot_week {
		for row in AnalyticsCosmeticSnapshot::find()
			.filter(analytics_cosmetic_snapshot::Column::WeekStart.eq(week))
			.all(&state.database)
			.await?
		{
			stock.insert(row.cosmetic_id, (row.owners, row.equipped));
		}
	}

	let cosmetics: Vec<CatalogEntry> = rows
		.into_iter()
		.map(|row| {
			let views = row.views.unwrap_or_default();
			let acquisitions = row.acquisitions.unwrap_or_default();

			CatalogEntry {
				name: names.get(&row.cosmetic_id).cloned().flatten(),
				cosmetic_id: row.cosmetic_id,
				views,
				acquisitions,
				acquisitions_paid: row.paid.unwrap_or_default(),
				acquisitions_granted: row.granted.unwrap_or_default(),
				#[expect(
					clippy::cast_precision_loss,
					reason = "counts are far below the f64 mantissa"
				)]
				conversion: (views > 0).then(|| acquisitions as f64 / views as f64),
				owners: stock.get(&row.cosmetic_id).map(|&(owners, _)| owners),
				equipped: stock.get(&row.cosmetic_id).map(|&(_, equipped)| equipped),
				shelf_rate: stock
					.get(&row.cosmetic_id)
					.filter(|&&(owners, _)| owners > 0)
					.map(|&(owners, equipped)| f64::from(equipped) / f64::from(owners)),
			}
		})
		.collect();

	Ok(Json(CatalogResponse {
		start,
		end,
		cosmetics,
	}))
}

