//! SQLite schema migrations for the exam service.

pub use sea_orm_migration::prelude::*;

mod m20261119_000001_initial;
mod m20261120_000002_simplify;
mod m20261120_154641_remove_audit_change_count;

/// Applies the exam schema migrations in order.
pub struct Migrator;

#[sea_orm_migration::async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261119_000001_initial::Migration),
            Box::new(m20261120_000002_simplify::Migration),
            Box::new(m20261120_154641_remove_audit_change_count::Migration),
        ]
    }
}
