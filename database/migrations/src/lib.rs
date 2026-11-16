pub use sea_orm_migration::prelude::*;

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
	fn migrations() -> Vec<Box<dyn MigrationTrait>> {
		discovered_migrations()
	}
}
