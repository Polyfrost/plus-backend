pub use sea_orm_migration::prelude::*;

mod m20250917_163702_create_users_table;
mod m20250917_163707_create_cosmetics_table;
mod m20250917_163717_create_usercosmetics_table;
mod m20250928_175510_create_cosmeticpackage_table;
mod m20251014_222424_add_active_cosmetics;
mod m20260601_235900_extend_cosmetic_type_enum;
mod m20260602_000000_cosmetics_realtime_schema;
mod m20260620_000000_create_monthly_active_login;
mod m20260625_000000_add_hat_cosmetic;
mod m20260625_000001_shared_cosmetic_emote_id_seq;
mod m20260625_000002_add_aura_cosmetic;
mod m20260625_000003_add_boots_cosmetic;
mod m20260625_000004_add_shoulder_cosmetic;
mod m20260625_000005_create_cosmetic_groups;
mod m20260628_000000_add_particle_color;
mod m20260629_000000_create_daily_playtime;
mod m20260701_000000_add_stripe_price_id;
mod m20260701_000001_add_stripe_transaction_provider;
mod m20260701_000002_extend_stripe_transactions;
mod m20260701_000003_add_transaction_recipient;
mod m20260703_000000_logic_gifts_rewrite;
mod m20260704_000000_create_collections_table;
mod m20260704_000001_extend_cosmetics_info;
mod m20260704_000002_extend_emotes_info;
mod m20260704_000003_add_bundle_tables;
mod m20260704_000004_drop_emotes;
mod m20260705_000000_create_tags_tables;
mod m20260705_000001_extend_user_payment_info;
mod m20260705_000002_extend_collections;
mod m20260708_000000_extend_tags_type;
mod m20260708_000001_extend_tags_table;
mod m20260710_000000_extend_cosmetics_info;
mod m20260711_000000_add_cosmetic_cover;
mod m20260717_000000_add_cosmetic_trigram_search;
mod m20260720_000000_create_tracked_links;
mod m20260729_000000_add_pet_cosmetic;
mod m20260731_000000_create_social_tables;
mod m20260731_000001_create_oidc_keys;
mod m20260731_000002_add_eos_product_user_id;
mod m20260804_000000_add_username_to_users;
mod m20260808_000000_create_special_chat_cooldowns;
mod m20260810_000000_add_session_invite_to_group_messages;
mod m20260811_000000_normalize_asset_hash_placeholder;
mod m20260815_000000_add_analytics_indexes;
mod m20260815_000001_create_analytics_rollup;
mod m20260815_000002_create_cohort_retention;
mod m20260815_000003_add_mau_indexes;
mod m20260815_000004_create_play_sessions;
mod m20260815_000005_create_session_lengths;
mod m20260815_000006_create_cosmetic_daily;
mod m20260815_000007_create_ownership_events;
mod m20260815_000008_transaction_amounts;
mod m20260815_000009_revenue_columns;
mod m20260815_000010_create_client_info;
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
	fn migrations() -> Vec<Box<dyn MigrationTrait>> {
		vec![
			Box::new(m20250917_163702_create_users_table::Migration),
			Box::new(m20250917_163707_create_cosmetics_table::Migration),
			Box::new(m20250917_163717_create_usercosmetics_table::Migration),
			Box::new(m20250928_175510_create_cosmeticpackage_table::Migration),
			Box::new(m20251014_222424_add_active_cosmetics::Migration),
			Box::new(m20260601_235900_extend_cosmetic_type_enum::Migration),
			Box::new(m20260602_000000_cosmetics_realtime_schema::Migration),
			Box::new(m20260620_000000_create_monthly_active_login::Migration),
			Box::new(m20260625_000000_add_hat_cosmetic::Migration),
			Box::new(m20260625_000001_shared_cosmetic_emote_id_seq::Migration),
			Box::new(m20260625_000002_add_aura_cosmetic::Migration),
			Box::new(m20260625_000003_add_boots_cosmetic::Migration),
			Box::new(m20260625_000004_add_shoulder_cosmetic::Migration),
			Box::new(m20260625_000005_create_cosmetic_groups::Migration),
			Box::new(m20260628_000000_add_particle_color::Migration),
			Box::new(m20260629_000000_create_daily_playtime::Migration),
			Box::new(m20260701_000000_add_stripe_price_id::Migration),
			Box::new(m20260701_000001_add_stripe_transaction_provider::Migration),
			Box::new(m20260701_000002_extend_stripe_transactions::Migration),
			Box::new(m20260701_000003_add_transaction_recipient::Migration),
			Box::new(m20260703_000000_logic_gifts_rewrite::Migration),
			Box::new(m20260704_000000_create_collections_table::Migration),
			Box::new(m20260704_000001_extend_cosmetics_info::Migration),
			Box::new(m20260704_000002_extend_emotes_info::Migration),
			Box::new(m20260704_000003_add_bundle_tables::Migration),
			Box::new(m20260704_000004_drop_emotes::Migration),
			Box::new(m20260705_000000_create_tags_tables::Migration),
			Box::new(m20260705_000001_extend_user_payment_info::Migration),
			Box::new(m20260705_000002_extend_collections::Migration),
			Box::new(m20260708_000000_extend_tags_type::Migration),
			Box::new(m20260708_000001_extend_tags_table::Migration),
			Box::new(m20260710_000000_extend_cosmetics_info::Migration),
			Box::new(m20260711_000000_add_cosmetic_cover::Migration),
			Box::new(m20260717_000000_add_cosmetic_trigram_search::Migration),
			Box::new(m20260720_000000_create_tracked_links::Migration),
			Box::new(m20260729_000000_add_pet_cosmetic::Migration),
			Box::new(m20260731_000000_create_social_tables::Migration),
			Box::new(m20260731_000001_create_oidc_keys::Migration),
			Box::new(m20260731_000002_add_eos_product_user_id::Migration),
			Box::new(m20260804_000000_add_username_to_users::Migration),
			Box::new(m20260808_000000_create_special_chat_cooldowns::Migration),
			Box::new(m20260810_000000_add_session_invite_to_group_messages::Migration),
			Box::new(m20260811_000000_normalize_asset_hash_placeholder::Migration),
			Box::new(m20260815_000000_add_analytics_indexes::Migration),
			Box::new(m20260815_000001_create_analytics_rollup::Migration),
			Box::new(m20260815_000002_create_cohort_retention::Migration),
			Box::new(m20260815_000003_add_mau_indexes::Migration),
			Box::new(m20260815_000004_create_play_sessions::Migration),
			Box::new(m20260815_000005_create_session_lengths::Migration),
			Box::new(m20260815_000006_create_cosmetic_daily::Migration),
			Box::new(m20260815_000007_create_ownership_events::Migration),
			Box::new(m20260815_000008_transaction_amounts::Migration),
			Box::new(m20260815_000009_revenue_columns::Migration),
			Box::new(m20260815_000010_create_client_info::Migration),
		]
	}
}
