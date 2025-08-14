pub use sea_orm_migration::prelude::*;

mod m20250810_210812_init;
mod m20250815_023013_create_users_table;
mod m20250815_023031_create_payments_table;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
	fn migrations() -> Vec<Box<dyn MigrationTrait>> {
		vec![
			Box::new(m20250810_210812_init::Migration),
			Box::new(m20250815_023013_create_users_table::Migration),
			Box::new(m20250815_023031_create_payments_table::Migration),
		]
	}
}
