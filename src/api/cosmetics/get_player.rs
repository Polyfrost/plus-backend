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
use entities::sea_orm_active_enums::CosmeticType;
use schemars::JsonSchema;
use sea_orm::{
	ColumnTrait as _, EntityTrait, QueryFilter, QuerySelect, QueryTrait, SelectColumns,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::api::{
	ApiState,
	account::OptionalAuthenticationExtractor,
	cosmetics::{ActiveCosmetics, CosmeticInfo},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("Authentication was not given, so a player query parameter is required")]
	PlayerRequired,
	#[error("Unable to fetch user data from database: {0}")]
	DatabaseFetch(#[from] sea_orm::error::DbErr),
	#[error("Unable to presign S3 URLs: {0}")]
	S3Presign(#[from] s3::error::S3Error),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("getPlayerCosmetics")
		.summary("Get a player's cosmetic status")
		.description(
			"Lists all cosmetics owned by a player, along with all active cosmetics",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"Authentication was not given, so a player query parameter is required",
			)
		})
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
		(
			match self {
				ResponseError::PlayerRequired => StatusCode::BAD_REQUEST,
				ResponseError::S3Presign(_) => StatusCode::INTERNAL_SERVER_ERROR,
				ResponseError::DatabaseFetch(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryParams {
	/// The UUID of the player to look up the cosmetics of. This is only
	/// optional if authentication is passed instead.
	#[serde(default)]
	#[schemars(example = &"f7c77d99-9f15-4a66-a87d-c4a51ef30d19")]
	player: Option<Uuid>,
}

/// Information about the player's cosmetics
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Response {
	cosmetics: Vec<CosmeticInfo>,
	active: ActiveCosmetics,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/player", get_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	OptionalAuthenticationExtractor(player): OptionalAuthenticationExtractor,
	Query(query): Query<QueryParams>,
) -> Result<Json<Response>, ResponseError> {
	let mut response = Response::default();
	let Some(player) = query.player.or(player) else {
		return Err(ResponseError::PlayerRequired);
	};

	{
		use entities::{prelude::*, user, user_cosmetic};

		let cosmetics = UserCosmetic::find()
			.filter(
				user_cosmetic::Column::User.in_subquery(
					// If this subquery contains no elements, the outer query will return
					// nothing
					User::find()
						.select_only()
						.select_column(user::Column::Id)
						.filter(user::Column::MinecraftUuid.eq(player))
						.limit(1)
						.into_query(),
				),
			)
			.find_also_related(Cosmetic)
			.all(&state.database)
			.await?;

		let mut tasks = JoinSet::new();
		for (user_cosmetic, cosmetic) in cosmetics
			.into_iter()
			.filter_map(|(uc, c)| c.map(|c| (uc, c)))
		{
			if user_cosmetic.active {
				match cosmetic.r#type {
					CosmeticType::Cape => response.active.cape = Some(cosmetic.id),
					CosmeticType::Emote => (),
				}
			}

			let cosmetic_cache = state.cosmetic_cache.clone();
			let s3_bucket = state.s3_bucket.clone();
			tasks.spawn(async move {
				CosmeticInfo::from_db_model(&cosmetic, cosmetic_cache, s3_bucket).await
			});
		}
		response.cosmetics.extend(
			tasks
				.join_all()
				.await
				.into_iter()
				.collect::<Result<Vec<_>, _>>()?,
		);
	};

	Ok(Json(response))
}
