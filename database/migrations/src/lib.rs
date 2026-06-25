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
		]
	}
}
