use sea_orm_migration::prelude::*;

mod initial;
mod simplify;

pub(crate) struct Migrator;

#[sea_orm_migration::async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(initial::Initial), Box::new(simplify::Simplify)]
    }
}
