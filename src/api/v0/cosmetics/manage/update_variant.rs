use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::sea_orm_active_enums::{BodySlot, CosmeticType};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait,
};
use serde::Deserialize;

use crate::api::{ApiState, admin_auth::AdminAuthenticationExtractor};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum UpdateVariantError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("At least one body slot must be provided")]
	MissingSlots,
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
}

impl IntoResponse for UpdateVariantError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::MissingSlots => StatusCode::BAD_REQUEST,
				Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			self,
		)
	}
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateVariantRequest {
	/// The id of the variant to update.
	cosmetic_id: i32,
	/// When present, sets (or clears with null) the variant label.
	#[serde(
		default,
		deserialize_with = "serde_with::rust::double_option::deserialize"
	)]
	variant_name: Option<Option<String>>,
	/// When present, sets (or clears with null) the skin model ("slim"/"wide").
	#[serde(
		default,
		deserialize_with = "serde_with::rust::double_option::deserialize"
	)]
	model_variant: Option<Option<String>>,
	/// When set, the variant's position within its group.
	variant_order: Option<i32>,
	/// When set, replaces the body slots this variant may be equipped in.
	/// Players equipping it in a removed slot are unequipped.
	slots: Option<Vec<BodySlot>>,
}

fn endpoint_doc(op: TransformOperation) -> TransformOperation {
	op.id("updateCosmeticVariant")
		.summary("Update a cosmetic variant")
		.description(
			"Updates a single variant's label, skin model, order, and allowed body \
			 slots without touching its siblings. Group-wide fields (name, price, \
			 description) are set from `/update`. Re-render the cover from \
			 `/render-cover` after changing the skin model. Admin password required.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The variant was updated")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No cosmetic exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Invalid or missing admin password")
		})
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route(
		"/update-variant",
		post_with(self::endpoint, self::endpoint_doc),
	)
}

#[tracing::instrument(level = "debug", skip(state, _auth))]
async fn endpoint(
	State(state): State<ApiState>,
	_auth: AdminAuthenticationExtractor,
	Json(body): Json<UpdateVariantRequest>,
) -> Result<StatusCode, UpdateVariantError> {
	use entities::{
		cosmetic, cosmetic_allowed_slot, cosmetic_group_allowed_slot,
		player_equipped_cosmetic, prelude::*,
	};

	let txn = state.database.begin().await?;

	let Some(cosmetic) = Cosmetic::find_by_id(body.cosmetic_id).one(&txn).await? else {
		return Err(UpdateVariantError::MissingCosmetic);
	};

	if let Some(slots) = &body.slots {
		if slots.is_empty() && cosmetic.r#type != CosmeticType::Emote {
			return Err(UpdateVariantError::MissingSlots);
		}

		CosmeticAllowedSlot::delete_many()
			.filter(cosmetic_allowed_slot::Column::CosmeticId.eq(cosmetic.id))
			.exec(&txn)
			.await?;
		if !slots.is_empty() {
			CosmeticAllowedSlot::insert_many(slots.iter().map(|slot| {
				cosmetic_allowed_slot::ActiveModel {
					cosmetic_id: Set(cosmetic.id),
					slot: Set(slot.clone()),
				}
			}))
			.exec(&txn)
			.await?;
		}

		// Additive like `/create`: siblings may still use the group's other slots.
		if let Some(group_id) = cosmetic.group_id
			&& !slots.is_empty()
		{
			CosmeticGroupAllowedSlot::insert_many(slots.iter().map(|slot| {
				cosmetic_group_allowed_slot::ActiveModel {
					group_id: Set(group_id),
					slot: Set(slot.clone()),
				}
			}))
			.on_conflict_do_nothing()
			.exec(&txn)
			.await?;
		}

		PlayerEquippedCosmetic::delete_many()
			.filter(player_equipped_cosmetic::Column::CosmeticId.eq(cosmetic.id))
			.filter(player_equipped_cosmetic::Column::Slot.is_not_in(slots.clone()))
			.exec(&txn)
			.await?;
	}

	let mut active: cosmetic::ActiveModel = cosmetic.into();
	if let Some(variant_name) = body.variant_name {
		active.variant_name = Set(variant_name);
	}
	if let Some(model_variant) = body.model_variant {
		active.model_variant = Set(model_variant);
	}
	if let Some(variant_order) = body.variant_order {
		active.variant_order = Set(variant_order);
	}
	if active.is_changed() {
		active.update(&txn).await?;
	}

	txn.commit().await?;

	Ok(StatusCode::NO_CONTENT)
}
