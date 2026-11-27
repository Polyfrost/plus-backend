use aide::axum::ApiRouter;

use crate::api::ApiState;

pub mod colors;
mod endpoint;
mod pet_preferences;
mod shared_emote;
pub mod structs;

pub(crate) use endpoint::{
	adopt_aura_color, announce_equipment_change, announce_particle_color_change, announce_presence,
	broadcast_all, broadcast_to_watchers, commit_look, commit_particle_color,
	send_to_owner, send_to_owner_connections, send_to_realtime_owner, change_allowed_slots, unequip_revoked,
	under_slot_rules,
};
pub(crate) use shared_emote::SharedEmoteRequests;

pub(super) async fn setup_router() -> ApiRouter<ApiState> {
	ApiRouter::new().merge(endpoint::router())
}
