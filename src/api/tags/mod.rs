mod create;
mod list;

use std::collections::HashMap;

use aide::axum::ApiRouter;
use entities::sea_orm_active_enums::TagType;
use schemars::JsonSchema;
use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};
use serde::Serialize;

use crate::api::ApiState;

/// The tags applied to a cosmetic, grouped by their type.
#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub(crate) struct CosmeticTags {
	/// The names of the `color` tags applied to this cosmetic.
	colors: Vec<String>,
	/// The names of the `custom` tags applied to this cosmetic.
	custom: Vec<String>,
}

/// Fetches the tags for each of the given cosmetic ids, grouped by type.
///
/// Cosmetics with no tags are absent from the map; callers should treat a
/// missing entry as an empty [`CosmeticTags`].
pub(crate) async fn tags_for_cosmetics(
	db: &DatabaseConnection,
	cosmetic_ids: &[i32],
) -> Result<HashMap<i32, CosmeticTags>, DbErr> {
	use entities::{prelude::*, tags_cosmetic};

	if cosmetic_ids.is_empty() {
		return Ok(HashMap::new());
	}

	let rows = TagsCosmetic::find()
		.filter(entities::tags::Column::TagType.ne(TagType::Category))
		.filter(tags_cosmetic::Column::CosmeticId.is_in(cosmetic_ids.iter().copied()))
		.find_also_related(Tags)
		.all(db)
		.await?;

	let mut grouped: HashMap<i32, CosmeticTags> = HashMap::new();
	for (link, tag) in rows {
		let Some(tag) = tag else {
			continue;
		};

		let entry = grouped.entry(link.cosmetic_id).or_default();
		match tag.tag_type {
			TagType::Color => entry.colors.push(tag.name),
			TagType::Custom => entry.custom.push(tag.name),
			TagType::Category => {}
		}
	}

	Ok(grouped)
}

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().nest(
		"/tags",
		ApiRouter::new()
			.merge(list::router())
			.merge(create::router()),
	)
}
