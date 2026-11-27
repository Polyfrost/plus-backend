use aide::{
	OperationIo,
	axum::{ApiRouter, routing::post_with},
	transform::TransformOperation,
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use entities::{
	cosmetic, player_outfit_cosmetic,
	prelude::*,
	sea_orm_active_enums::{BodySlot, CosmeticType},
	user,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QuerySelect, QueryTrait, Select,
	Set, TransactionTrait, sea_query::LockType,
};
use serde::Deserialize;

use crate::api::{
	ApiState,
	permissions::{Require, perm},
	v0::{
		bonuses::sets_changed,
		cosmetics::effect::{EffectSlotError, check_slots},
		websocket::change_allowed_slots,
	},
};

#[derive(thiserror::Error, Debug, OperationIo)]
pub enum UpdateVariantError {
	#[error("The requested cosmetic does not exist")]
	MissingCosmetic,
	#[error("At least one body slot must be provided")]
	MissingSlots,
	#[error("Nameplates take exactly one body slot, `nameplate`, and no other type can use it")]
	InvalidNameplateSlots,
	#[error("Another variant in this group already uses the variant key {0:?}")]
	DuplicateVariantKey(String),
	#[error(transparent)]
	EffectSlots(#[from] EffectSlotError),
	#[error("Database error: {0}")]
	Database(#[from] sea_orm::error::DbErr),
	#[error(transparent)]
	Nameplate(#[from] crate::api::v0::cosmetics::nameplate::NameplateBundleError),
	#[error("S3 error: {0}")]
	S3(#[from] s3::error::S3Error),
}

impl From<crate::api::v0::cosmetics::nameplate::StoredBundleError>
	for UpdateVariantError
{
	fn from(error: crate::api::v0::cosmetics::nameplate::StoredBundleError) -> Self {
		use crate::api::v0::cosmetics::nameplate::StoredBundleError;
		match error {
			StoredBundleError::Bundle(error) => Self::Nameplate(error),
			StoredBundleError::S3(error) => Self::S3(error),
		}
	}
}

impl IntoResponse for UpdateVariantError {
	fn into_response(self) -> axum::response::Response {
		crate::api::admin_error_response(
			match self {
				Self::MissingCosmetic => StatusCode::NOT_FOUND,
				Self::DuplicateVariantKey(_) => StatusCode::CONFLICT,
				Self::MissingSlots
				| Self::InvalidNameplateSlots
				| Self::EffectSlots(_)
				| Self::Nameplate(_) => StatusCode::BAD_REQUEST,
				Self::Database(_) | Self::S3(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
	#[serde(
		default,
		deserialize_with = "serde_with::rust::double_option::deserialize"
	)]
	variant_key: Option<Option<String>>,
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
			 `/render-cover` after changing the skin model. Requires `MANAGE_COSMETICS`.",
		)
		.tag("cosmetics")
		.response_with::<{ StatusCode::NO_CONTENT.as_u16() }, (), _>(|res| {
			res.description("The variant was updated")
		})
		.response_with::<{ StatusCode::NOT_FOUND.as_u16() }, String, _>(|res| {
			res.description("No cosmetic exists with the given id")
		})
		.response_with::<{ StatusCode::UNAUTHORIZED.as_u16() }, String, _>(|res| {
			res.description("Missing or invalid credentials, or lacking the permission")
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
	_auth: Require<{ perm::MANAGE_COSMETICS }>,
	Json(body): Json<UpdateVariantRequest>,
) -> Result<StatusCode, UpdateVariantError> {
	let cosmetic_id = body.cosmetic_id;
	let rematch_sets = match body.slots.clone() {
		Some(slots) => {
			change_allowed_slots(&state, cosmetic_id, &slots, update(&state, body)).await?
		}
		None => update(&state, body).await?,
	};

	crate::api::v0::cosmetics::notify_catalog_updated(&state);
	if rematch_sets {
		sets_changed(&state).await;
	}

	Ok(StatusCode::NO_CONTENT)
}

fn locked_for_reslotting(cosmetic_id: i32) -> Select<Cosmetic> {
	Cosmetic::find_by_id(cosmetic_id).lock(LockType::NoKeyUpdate)
}

fn active_outfits_wearing_outside(
	cosmetic_id: i32,
	slots: &[BodySlot],
) -> Select<PlayerOutfitCosmetic> {
	PlayerOutfitCosmetic::find()
		.filter(player_outfit_cosmetic::Column::CosmeticId.eq(cosmetic_id))
		.filter(player_outfit_cosmetic::Column::Slot.is_not_in(slots.to_vec()))
		.filter(
			player_outfit_cosmetic::Column::OutfitId.in_subquery(
				User::find()
					.select_only()
					.column(user::Column::ActiveOutfitId)
					.filter(user::Column::ActiveOutfitId.is_not_null())
					.into_query(),
			),
		)
}

async fn update(
	state: &ApiState,
	body: UpdateVariantRequest,
) -> Result<bool, UpdateVariantError> {
	use entities::{
		cosmetic_allowed_slot, cosmetic_group_allowed_slot, player_equipped_cosmetic,
		player_outfit,
	};
	use sea_orm::sea_query::Expr;

	let txn = state.database.begin().await?;

	let Some(cosmetic) = locked_for_reslotting(body.cosmetic_id).one(&txn).await? else {
		return Err(UpdateVariantError::MissingCosmetic);
	};
	let rematch_sets = changes_set_matching(cosmetic.group_id, &body);

	if let Some(Some(variant_key)) = &body.variant_key
		&& cosmetic.r#type == CosmeticType::Nameplate
		&& let Some(asset_id) = cosmetic.asset_id
		&& let Some(asset) = Asset::find_by_id(asset_id).one(&txn).await?
		&& let Some((bundle, _)) =
			crate::api::v0::cosmetics::nameplate::stored_bundle(&state, &asset).await?
	{
		bundle.check_variant_key(Some(variant_key))?;
	}

	if let Some(Some(variant_key)) = &body.variant_key
		&& let Some(group_id) = cosmetic.group_id
		&& super::create::variant_key_taken(&txn, group_id, variant_key, Some(cosmetic.id))
			.await?
	{
		return Err(UpdateVariantError::DuplicateVariantKey(variant_key.clone()));
	}

	if let Some(slots) = &body.slots {
		if slots.is_empty() && cosmetic.r#type != CosmeticType::Emote {
			return Err(UpdateVariantError::MissingSlots);
		}
		if !super::create::nameplate_slots_valid(&cosmetic.r#type, slots) {
			return Err(UpdateVariantError::InvalidNameplateSlots);
		}
		check_slots(&cosmetic.r#type, slots)?;

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

		let stale = active_outfits_wearing_outside(cosmetic.id, slots)
			.all(&txn)
			.await?;
		if !stale.is_empty() {
			let mut outfits = stale.iter().map(|entry| entry.outfit_id).collect::<Vec<_>>();
			outfits.sort_unstable();
			outfits.dedup();
			PlayerOutfitCosmetic::delete_many()
				.filter(player_outfit_cosmetic::Column::CosmeticId.eq(cosmetic.id))
				.filter(player_outfit_cosmetic::Column::Slot.is_not_in(slots.clone()))
				.filter(player_outfit_cosmetic::Column::OutfitId.is_in(outfits.clone()))
				.exec(&txn)
				.await?;
			PlayerOutfit::update_many()
				.col_expr(player_outfit::Column::UpdatedAt, Expr::current_timestamp().into())
				.filter(player_outfit::Column::Id.is_in(outfits))
				.exec(&txn)
				.await?;
		}
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
	if let Some(variant_key) = body.variant_key {
		active.variant_key = Set(variant_key);
	}
	if active.is_changed() {
		active.update(&txn).await?;
	}

	txn.commit().await?;

	Ok(rematch_sets)
}

fn changes_set_matching(group_id: Option<i32>, body: &UpdateVariantRequest) -> bool {
	group_id.is_some() && body.variant_key.is_some()
}

#[cfg(test)]
mod tests {
	use entities::sea_orm_active_enums::BodySlot;
	use sea_orm::{DbBackend, QueryTrait};

	use super::{
		UpdateVariantRequest, active_outfits_wearing_outside, changes_set_matching,
		locked_for_reslotting,
	};

	#[test]
	fn rekeying_rematches_sets_and_reslotting_locks_and_finds_active_outfits() {
		let request = |key: Option<Option<&str>>| UpdateVariantRequest {
			cosmetic_id: 1,
			variant_name: Some(Some("Red".to_owned())),
			model_variant: None,
			variant_order: Some(2),
			variant_key: key.map(|key| key.map(str::to_owned)),
			slots: None,
		};
		assert!(changes_set_matching(Some(4), &request(Some(Some("red")))));
		assert!(changes_set_matching(Some(4), &request(Some(None))));
		assert!(!changes_set_matching(Some(4), &request(None)));
		assert!(!changes_set_matching(None, &request(Some(Some("red")))));

		let locked = locked_for_reslotting(5).build(DbBackend::Postgres).to_string();
		assert!(locked.ends_with("FOR NO KEY UPDATE"), "{locked}");
		let sql = active_outfits_wearing_outside(5, &[BodySlot::Hat]).build(DbBackend::Postgres).to_string();
		for part in [
			r#""player_outfit_cosmetic"."cosmetic_id" = 5"#,
			r#""player_outfit_cosmetic"."slot" NOT IN"#,
			r#""player_outfit_cosmetic"."outfit_id" IN (SELECT "user"."active_outfit_id" FROM "user" WHERE "user"."active_outfit_id" IS NOT NULL)"#,
		] {
			assert!(sql.contains(part), "{sql}");
		}
	}
}
