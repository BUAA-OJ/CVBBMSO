use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub(super) struct Migration;

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Migration {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild(manager, "write_id INTEGER PRIMARY KEY,", "UNIQUE").await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild(manager, "", "PRIMARY KEY").await
    }
}

async fn rebuild(
    manager: &SchemaManager<'_>,
    write_id: &str,
    student_key: &str,
) -> Result<(), DbErr> {
    let db = manager.get_connection();
    db.execute_unprepared(&format!(
        "CREATE TABLE events_next (
            {write_id}
            exam_id TEXT NOT NULL,
            oj_user_id INTEGER NOT NULL CHECK (oj_user_id > 0),
            seq INTEGER NOT NULL CHECK (typeof(seq) = 'integer' AND seq >= 1),
            event TEXT NOT NULL,
            received_at TEXT NOT NULL,
            {student_key} (exam_id, oj_user_id, seq),
            FOREIGN KEY (exam_id, oj_user_id) REFERENCES student_runtimes (exam_id, oj_user_id)
        )"
    ))
    .await?;
    db.execute_unprepared(
        "INSERT INTO events_next (exam_id, oj_user_id, seq, event, received_at)
         SELECT exam_id, oj_user_id, seq, event, received_at FROM events",
    )
    .await?;
    db.execute_unprepared("DROP TABLE events").await?;
    db.execute_unprepared("ALTER TABLE events_next RENAME TO events")
        .await?;
    Ok(())
}
