use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub(super) struct Migration;

const RUNTIMES: &str = "student_runtimes";
const CONNECTION_ID: &str = "connection_id";

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Migration {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RUNTIMES)
                    .add_column(ColumnDef::new(CONNECTION_ID).text())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RUNTIMES)
                    .drop_column(CONNECTION_ID)
                    .to_owned(),
            )
            .await
    }
}
