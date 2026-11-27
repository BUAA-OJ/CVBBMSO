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
                "CREATE TABLE student_enrollments (
                    exam_id TEXT NOT NULL REFERENCES exams(exam_id) ON DELETE CASCADE,
                    student_number TEXT NOT NULL CHECK (
                        length(student_number) BETWEEN 1 AND 20
                        AND student_number NOT GLOB '*[^0-9]*'
                    ),
                    oj_user_id INTEGER,
                    enrolled INTEGER NOT NULL CHECK (enrolled IN (0, 1)),
                    PRIMARY KEY (exam_id, student_number),
                    UNIQUE (exam_id, oj_user_id)
                )",
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_student_enrollments_number")
                    .table("student_enrollments")
                    .col("student_number")
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table("student_enrollments").to_owned())
            .await
    }
}
