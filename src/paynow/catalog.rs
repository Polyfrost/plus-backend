/// Derived from the row id, not the name, so a rename cannot orphan the
/// product and a re-run after a crash finds the one that already exists.
pub(crate) fn cosmetic_group_slug(group_id: i32) -> String {
	format!("cosmetic-group-{group_id}")
}

pub(crate) fn cosmetic_slug(cosmetic_id: i32) -> String {
	format!("cosmetic-{cosmetic_id}")
}

pub(crate) fn bundle_slug(bundle_id: i32) -> String {
	format!("bundle-{bundle_id}")
}
