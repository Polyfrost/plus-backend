use std::collections::HashSet;

use aide::{
	OperationIo,
	axum::{ApiRouter, routing::put_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, NoContent},
};
use entities::sea_orm_active_enums::{CosmeticType, CosmeticTypeEnum};
use migrations::Expr;
use schemars::JsonSchema;
use sea_orm::{
	ActiveEnum, ColumnTrait, EntityTrait, IntoSimpleExpr, QueryFilter, QuerySelect,
	QueryTrait, SelectColumns as _, TransactionTrait,
};
use serde::Deserialize;

use crate::api::{
	ApiState, account::AuthenticationExtractor, cosmetics::PartialActiveCosmetics,
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum ResponseError {
	#[error("The given ID {id} is invalid for cosmetic type {cosmetic_type}")]
	InvalidId { cosmetic_type: String, id: i32 },
	#[error("Unable to query database: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("putCosmetics")
		.summary("Set a player's active cosmetics")
		.description("Sets the authorized player's active cosmetics")
		.tag("cosmetics")
		.response_with::<{ StatusCode::BAD_REQUEST.as_u16() }, String, _>(|res| {
			res.description(
				"An error given when a passed ID is invalid for a given cosmetic type",
			)
			.example("The given ID 2 is invalid for cosmetic type emote")
		})
		.response_with::<{ StatusCode::INTERNAL_SERVER_ERROR.as_u16() }, String, _>(
			|res| {
				res.description(
					"An internal server error occurred while trying to set active \
					 cosmetics",
				)
			},
		)
}

impl IntoResponse for ResponseError {
	fn into_response(self) -> axum::response::Response {
		(
			match self {
				ResponseError::InvalidId { .. } => StatusCode::BAD_REQUEST,
				ResponseError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self.to_string(),
		)
			.into_response()
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RequestBody {
	/// An object of cosmetic types to the active one.
	active: PartialActiveCosmetics,
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/player", put_with(self::endpoint, self::endpoint_doc))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn endpoint(
	State(state): State<ApiState>,
	AuthenticationExtractor(player): AuthenticationExtractor,
	Json(body): Json<RequestBody>,
) -> Result<NoContent, ResponseError> {
	{
		use entities::{cosmetic, prelude::*, user, user_cosmetic};

		let txn = state.database.begin().await?;

		let correct_cosmetics = Cosmetic::find()
			.select_only()
			.columns([cosmetic::Column::Id, cosmetic::Column::Type])
			.filter(
				Expr::tuple([
					cosmetic::Column::Id.into_simple_expr(),
					cosmetic::Column::Type.into_simple_expr(),
				])
				.is_in(body.active.into_iter().filter_map(|(name, value)| {
					Some(Expr::tuple([
						value?.into(),
						Expr::val(name).cast_as(CosmeticTypeEnum),
					]))
				})),
			)
			.distinct()
			.into_tuple()
			.all(&txn)
			.await?
			.into_iter()
			.collect::<HashSet<(i32, CosmeticType)>>();

		for (name, value) in body.active.into_iter() {
			// Ensure this id exists
			let name_string = name.to_string();
			if let Some(id) = value
				&& !correct_cosmetics.contains(&(
					id,
					CosmeticType::try_from_value(&name_string)
						.expect("Should always suceed"),
				)) {
				return Err(ResponseError::InvalidId {
					cosmetic_type: name_string,
					id,
				});
			}

			UserCosmetic::update_many()
				.col_expr(
					user_cosmetic::Column::Active,
					if let Some(id) = value {
						user_cosmetic::Column::Cosmetic.eq(id)
					} else {
						false.into()
					},
				)
				.filter(
					user_cosmetic::Column::Cosmetic.in_subquery(
						Cosmetic::find()
							.select_only()
							.column(cosmetic::Column::Id)
							.filter(cosmetic::Column::Type.eq(name))
							.into_query(),
					),
				)
				.filter(
					user_cosmetic::Column::User.in_subquery(
						User::find()
							.select_only()
							.select_column(user::Column::Id)
							.filter(user::Column::MinecraftUuid.eq(player))
							.into_query(),
					),
				)
				.exec(&txn)
				.await?;
		}

		txn.commit().await?;
	}

	Ok(NoContent)
}
