use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub(super) struct Migration;

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Migration {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE student_profiles (
                    oj_user_id INTEGER PRIMARY KEY CHECK (oj_user_id > 0),
                    student_number TEXT,
                    nickname TEXT
                )",
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("student_runtimes_by_user")
                    .table("student_runtimes")
                    .col("oj_user_id")
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("student_runtimes_by_user")
                    .table("student_runtimes")
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table("student_profiles").to_owned())
            .await
    }
}
