mod get_player;
mod put_player;

use aide::axum::ApiRouter;
use entities::sea_orm_active_enums::CosmeticType;
use s3::error::S3Error;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::ApiState;

#[derive(Debug, Serialize, JsonSchema)]
struct CosmeticInfo {
	/// The unique ID of this cosmetic
	id: i32,
	/// The type of this cosmetic
	r#type: CosmeticType,
	/// The media url for this cosmetic
	#[serde(skip_serializing_if = "Option::is_none")]
	url: Option<String>
}

impl CosmeticInfo {
	async fn from_db(
		model: entities::cosmetic::Model,
		bucket: &s3::Bucket
	) -> Result<Self, S3Error> {
		Ok(Self {
			id: model.id,
			r#type: model.r#type.clone(),
			url: match &model.path {
				Some(p) => Some(bucket.presign_get(p, 604800, None).await?),
				_ => None
			}
		})
	}
}

macro_rules! gen_active_cosmetics_structs {
	($($name:ident),+) => {
		#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
		pub struct ActiveCosmetics {
			$(
				#[doc = "The ID of the active "]
				#[doc = stringify!($name)]
				#[doc = ", or null to disable"]
				$name: Option<i32>
			),+
		}

		impl ActiveCosmetics {
			pub const NAMES: [&str; { [$(stringify!($name)),+].len() }] = [$(stringify!($name)),+];
		}

		/// An object of partially-set active cosmetics.
		///
		/// Omitting a key keeps it the same, using `null` unsets it, and passing a value sets that as the active.
		#[derive(Debug, Serialize, Deserialize, JsonSchema)]
		pub struct PartialActiveCosmetics {
			$(
				#[doc = "The ID of the active "]
				#[doc = stringify!($name)]
				#[doc = ", or null to disable"]
				#[serde(
					default,
					skip_serializing_if = "Option::is_none",
					serialize_with = "::serde_with::rust::double_option::serialize",
					deserialize_with = "::serde_with::rust::double_option::deserialize",
				)]
				$name: Option<Option<i32>>
			),+
		}

		impl IntoIterator for &PartialActiveCosmetics {
			type Item = (&'static str, Option<i32>);
			type IntoIter = std::iter::Flatten<
				std::array::IntoIter<Option<(&'static str, Option<i32>)>, { ActiveCosmetics::NAMES.len() }>
			>;

			fn into_iter(self) -> Self::IntoIter {
				[$(self.$name.map(|v| (stringify!($name), v))),+].into_iter().flatten()
			}
		}
	};
}

gen_active_cosmetics_structs!(cape);

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.merge(get_player::router())
		.merge(put_player::router())
}
