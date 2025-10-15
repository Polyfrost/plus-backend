pub use sea_orm_migration::prelude::*;

mod m20250917_163702_create_users_table;
mod m20250917_163707_create_cosmetics_table;
mod m20250917_163717_create_usercosmetics_table;
mod m20250928_175510_create_cosmeticpackage_table;
mod m20251014_222424_add_active_cosmetics;

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
		]
	}
}
