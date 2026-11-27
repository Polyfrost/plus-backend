use entities::sea_orm_active_enums::{BodySlot, CosmeticType};

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub(crate) enum EffectSlotError {
	#[error("Effect cosmetics only go in the effect_a and effect_b slots")]
	EffectOutsideEffectSlots,
	#[error("Only effect cosmetics go in the effect_a and effect_b slots")]
	EffectSlotForOtherType,
}

pub(crate) fn is_effect_slot(slot: &BodySlot) -> bool {
	matches!(slot, BodySlot::EffectA | BodySlot::EffectB)
}

pub(crate) fn check_slots(
	cosmetic_type: &CosmeticType,
	slots: &[BodySlot],
) -> Result<(), EffectSlotError> {
	if matches!(cosmetic_type, CosmeticType::Effect) {
		if slots.iter().all(is_effect_slot) {
			Ok(())
		} else {
			Err(EffectSlotError::EffectOutsideEffectSlots)
		}
	} else if slots.iter().any(is_effect_slot) {
		Err(EffectSlotError::EffectSlotForOtherType)
	} else {
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use entities::sea_orm_active_enums::{BodySlot, CosmeticType};

	use super::{EffectSlotError, check_slots};

	#[test]
	fn effect_names_round_trip_and_only_effects_take_effect_slots() {
		for (slot, name) in [(BodySlot::EffectA, "effect_a"), (BodySlot::EffectB, "effect_b")] {
			assert_eq!(serde_json::to_value(&slot).unwrap(), name);
			assert_eq!(serde_json::from_value::<BodySlot>(name.into()).unwrap(), slot);
			assert_eq!(sea_orm::ActiveEnum::to_value(&slot), name);
		}
		assert_eq!(serde_json::to_value(CosmeticType::Effect).unwrap(), "effect");
		assert_eq!(serde_json::from_value::<CosmeticType>("effect".into()).unwrap(), CosmeticType::Effect);
		assert_eq!(sea_orm::ActiveEnum::to_value(&CosmeticType::Effect), "effect");

		use BodySlot::{Aura, EffectA, EffectB};
		use CosmeticType as T;
		for (kind, slots, expected) in [
			(T::Effect, &[EffectA][..], Ok(())),
			(T::Effect, &[EffectB], Ok(())),
			(T::Effect, &[EffectA, EffectB], Ok(())),
			(T::Effect, &[EffectA, Aura], Err(EffectSlotError::EffectOutsideEffectSlots)),
			(T::Aura, &[Aura], Ok(())),
			(T::Aura, &[Aura, EffectB], Err(EffectSlotError::EffectSlotForOtherType)),
			(T::Pet, &[EffectA], Err(EffectSlotError::EffectSlotForOtherType)),
		] {
			assert_eq!(check_slots(&kind, slots), expected, "{kind:?} {slots:?}");
		}
	}
}
