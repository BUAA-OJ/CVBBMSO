//! SQLite schema migrations for the exam service.

pub use sea_orm_migration::prelude::*;

mod m20261121_043144_initial;

/// Applies the exam schema migrations in order.
pub struct Migrator;

#[sea_orm_migration::async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20261121_043144_initial::Migration)]
    }
}
