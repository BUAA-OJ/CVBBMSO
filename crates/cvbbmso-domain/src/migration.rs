use sea_orm_migration::prelude::*;

pub(crate) struct Migrator;

#[sea_orm_migration::async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(Initial)]
    }
}

struct Initial;

impl MigrationName for Initial {
    fn name(&self) -> &'static str {
        "m20261119_000001_initial"
    }
}

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Initial {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(include_str!("migration/up.sql"))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(include_str!("migration/down.sql"))
            .await?;
        Ok(())
    }
}
