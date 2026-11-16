use std::collections::HashSet;

use chrono::{Duration, Utc};
use entities::{cosmetic, discount, prelude::*, user};
use rand::random_range;
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter,
	Set, prelude::DateTimeWithTimeZone,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;
use uuid::Uuid;

use super::EventError;
use crate::{
	api::{
		ApiState,
		v0::cosmetics::grant::{grant_in, notify},
		v1::discounts::{
			DiscountBody, enabled_by_default, insert, mirror, validate as validate_discount,
		},
	},
	pricing::{Targets, normalise_code},
	utils::validation::valid_http_url,
};

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// 50 bits from the OS-seeded CSPRNG. The id part before it is guessable,
/// so this alone is what keeps a code secret.
const RANDOM_LENGTH: usize = 10;
const MAX_PREFIX_LENGTH: usize = 12;

/// What claiming a slot hands out. New kinds of reward are new variants.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Reward {
	/// Owned for good. A grouped cosmetic grants its whole group.
	Cosmetic { cosmetic_id: i32 },
	/// A single-use coupon generated for each player who claims it.
	Coupon(CouponReward),
	/// Handed out outside PolyPlus, e.g. Discord Nitro: staff fulfil it from
	/// the event's claim list.
	External {
		name: String,
		/// Shown to the player, e.g. how to collect it.
		message: String,
		/// An image, video or icon shown with it.
		#[serde(default)]
		preview_url: Option<String>,
	},
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub(super) struct CouponReward {
	/// Shown to the player and on the coupon list.
	name: String,
	/// Start of every generated code, e.g. `XMAS` for `XMAS-7KQ2M9TA`.
	prefix: String,
	/// Whole percent off. Exactly one of this and `amount_off_minor`.
	#[serde(default)]
	percent_off: Option<i32>,
	#[serde(default)]
	amount_off_minor: Option<i64>,
	/// Required with `amount_off_minor`.
	#[serde(default)]
	currency: Option<String>,
	#[serde(default)]
	applies_to_all: bool,
	/// Takes a fixed amount off each covered line rather than once off the order.
	#[serde(default = "enabled_by_default")]
	apply_individually: bool,
	#[serde(default)]
	targets: Targets,
	#[serde(default)]
	min_subtotal_minor: Option<i64>,
	/// Days a generated code works for once claimed. Forever when left out.
	#[serde(default)]
	valid_days: Option<u16>,
}

/// What a player received from a claim, kept so they can see it again.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Granted {
	/// The whole group when the cosmetic belongs to one, emotes listed apart.
	Cosmetic {
		cosmetic_ids: Vec<i32>,
		emote_ids: Vec<i32>,
	},
	Coupon {
		code: String,
		discount_id: i32,
		expires_at: Option<DateTimeWithTimeZone>,
	},
	External {
		name: String,
		message: String,
		preview_url: Option<String>,
	},
}

impl CouponReward {
	fn discount(
		&self,
		ends_at: Option<DateTimeWithTimeZone>,
		description: Option<String>,
	) -> DiscountBody {
		DiscountBody {
			name: self.name.clone(),
			description,
			code: None,
			percent_off: self.percent_off,
			amount_off_minor: self.amount_off_minor,
			currency: self.currency.clone(),
			starts_at: None,
			ends_at,
			enabled: true,
			applies_to_all: self.applies_to_all,
			apply_individually: self.apply_individually,
			targets: self.targets.clone(),
			min_subtotal_minor: self.min_subtotal_minor,
			max_redemptions: Some(1),
			max_per_player: Some(1),
		}
	}
}

pub(super) async fn validate(
	db: &impl ConnectionTrait,
	rewards: impl IntoIterator<Item = &Reward>,
) -> Result<(), EventError> {
	let mut cosmetic_ids = HashSet::new();
	for reward in rewards {
		match reward {
			Reward::Cosmetic { cosmetic_id } => {
				cosmetic_ids.insert(*cosmetic_id);
			}
			Reward::Coupon(coupon) => {
				if coupon.prefix.is_empty()
					|| coupon.prefix.len() > MAX_PREFIX_LENGTH
					|| !coupon.prefix.chars().all(|c| c.is_ascii_alphanumeric())
				{
					return Err(EventError::Invalid(
						"A coupon prefix must be 1 to 12 letters or digits",
					));
				}
				validate_discount(&coupon.discount(None, None))?;
			}
			Reward::External {
				name, preview_url, ..
			} => {
				if name.trim().is_empty() {
					return Err(EventError::Invalid("An external reward needs a name"));
				}
				if preview_url.as_deref().is_some_and(|url| !valid_http_url(url)) {
					return Err(EventError::Invalid("A preview must be an http(s) URL"));
				}
			}
		}
	}

	let found = Cosmetic::find()
		.filter(cosmetic::Column::Id.is_in(cosmetic_ids.iter().copied()))
		.all(db)
		.await?;
	if found.len() != cosmetic_ids.len() {
		return Err(EventError::Invalid("A reward cosmetic does not exist"));
	}

	Ok(())
}

/// Hands `reward` to `player` inside `txn`. Call [`deliver`] once it commits.
pub(super) async fn grant(
	txn: &DatabaseTransaction,
	player: &user::Model,
	reward: &Reward,
	metadata: Value,
) -> Result<Granted, EventError> {
	match reward {
		Reward::Cosmetic { cosmetic_id } => {
			let (cosmetic_ids, emote_ids) = grant_in(txn, player.id, *cosmetic_id, metadata).await?;
			Ok(Granted::Cosmetic {
				cosmetic_ids,
				emote_ids,
			})
		}
		Reward::Coupon(coupon) => {
			let expires_at = coupon
				.valid_days
				.map(|days| Utc::now().fixed_offset() + Duration::days(days.into()));
			let description = format!("Event reward for {}: {metadata}", player.minecraft_uuid);
			// Saved first so the code can be built from its id.
			let model = insert(txn, &coupon.discount(expires_at, Some(description)), None).await?;
			let discount_id = model.id;
			let code = code(&coupon.prefix, discount_id);
			let mut model: discount::ActiveModel = model.into();
			model.code = Set(Some(code.clone()));
			model.update(txn).await?;
			Ok(Granted::Coupon {
				code,
				discount_id,
				expires_at,
			})
		}
		Reward::External {
			name,
			message,
			preview_url,
		} => Ok(Granted::External {
			name: name.clone(),
			message: message.clone(),
			preview_url: preview_url.clone(),
		}),
	}
}

/// What has to happen outside the database once a grant commits. Failures are
/// logged rather than returned: the claim already stands.
pub(super) async fn deliver(
	state: &ApiState,
	player_uuid: Uuid,
	reward: &Reward,
	granted: &Granted,
) {
	match (reward, granted) {
		(
			_,
			Granted::Cosmetic {
				cosmetic_ids,
				emote_ids,
			},
		) => {
			notify(state, player_uuid, cosmetic_ids, emote_ids, false).await;
		}
		(Reward::Coupon(coupon), Granted::Coupon { discount_id, .. }) => {
			let mirrored = match Discount::find_by_id(*discount_id).one(&state.database).await {
				Ok(Some(model)) => mirror(state, None, model, &coupon.targets).await.err(),
				Ok(None) => None,
				Err(error) => Some(error.into()),
			};
			if let Some(error) = mirrored {
				warn!(%error, discount_id, "event coupon not mirrored to PayNow");
			}
		}
		_ => {}
	}
}

/// `PREFIX-` then the discount id in the code alphabet, then a random tail
/// so codes cannot be guessed from the id.
fn code(prefix: &str, discount_id: i32) -> String {
	let base = CODE_ALPHABET.len();
	let mut rest = usize::try_from(discount_id).unwrap_or_default();
	let mut code = format!("{prefix}-");
	loop {
		code.push(char::from(CODE_ALPHABET[rest % base]));
		rest /= base;
		if rest == 0 {
			break;
		}
	}
	code.extend((0..RANDOM_LENGTH).map(|_| char::from(CODE_ALPHABET[random_range(0..base)])));
	normalise_code(&code)
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use super::{CODE_ALPHABET, RANDOM_LENGTH, code};

	#[test]
	fn codes_from_different_discounts_never_match() {
		let ids: HashSet<String> = (1..=5000)
			.map(|id| {
				let code = code("xmas", id);
				assert!(code.starts_with("XMAS-"), "{code}");
				assert!(code[5..].bytes().all(|byte| CODE_ALPHABET.contains(&byte)), "{code}");
				code[..code.len() - RANDOM_LENGTH].to_owned()
			})
			.collect();
		assert_eq!(ids.len(), 5000);
	}
}
