use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub(super) struct Migration;

// Keep this schema independent of entities that may change in future migrations.
const EXAMS: &str = "exams";
const ENROLLMENTS: &str = "enrollments";
const RUNTIMES: &str = "student_runtimes";
const COMMANDS: &str = "commands";
const EVENTS: &str = "events";
const INCIDENTS: &str = "incidents";
const AUDIT: &str = "admin_audit";

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Migration {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for table in [
            exams(),
            enrollments(),
            runtimes(),
            commands(),
            events(),
            incidents(),
            audit(),
        ] {
            manager.create_table(table).await?;
        }
        for index in indexes() {
            manager.create_index(index).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for table in [
            AUDIT,
            INCIDENTS,
            EVENTS,
            COMMANDS,
            RUNTIMES,
            ENROLLMENTS,
            EXAMS,
        ] {
            manager
                .drop_table(Table::drop().table(table).take())
                .await?;
        }
        Ok(())
    }
}

fn exams() -> TableCreateStatement {
    Table::create()
        .table(EXAMS)
        .col(text("exam_id").primary_key())
        .col(text("title"))
        .col(text("start_at"))
        .col(text("end_at").check(Expr::col("start_at").lt(Expr::col("end_at"))))
        .col(policy_value("segment_duration_secs"))
        .col(policy_value("fps"))
        .to_owned()
}

fn enrollments() -> TableCreateStatement {
    Table::create()
        .table(ENROLLMENTS)
        .col(text("exam_id"))
        .col(user_id())
        .primary_key(Index::create().col("exam_id").col("oj_user_id"))
        .foreign_key(&mut exam_foreign_key(ENROLLMENTS))
        .to_owned()
}

fn runtimes() -> TableCreateStatement {
    Table::create()
        .table(RUNTIMES)
        .col(text("exam_id"))
        .col(user_id())
        .col(text("device_id"))
        .col(text("instance_id"))
        .col(integer("connected").check(Expr::col("connected").is_in([0, 1])))
        .col(text("execution").check(Expr::col("execution").is_in([
            "ready",
            "recording",
            "finalizing",
            "finished",
        ])))
        .col(text("recording").check(Expr::col("recording").is_in([
            "idle",
            "capturing",
            "finalizing",
            "complete",
            "failed",
        ])))
        .col(text("last_seen_at"))
        .col(sequence("ack_event_through", 0))
        .primary_key(Index::create().col("exam_id").col("oj_user_id"))
        .foreign_key(&mut exam_foreign_key(RUNTIMES))
        .take()
}

fn commands() -> TableCreateStatement {
    Table::create()
        .table(COMMANDS)
        .col(text("command_id").primary_key())
        .col(text("exam_id"))
        .col(user_id())
        .col(text("name").check(Expr::col("name").is_in(["start_exam", "end_exam"])))
        .col(text("result"))
        .foreign_key(&mut runtime_foreign_key(COMMANDS))
        .take()
}

fn events() -> TableCreateStatement {
    Table::create()
        .table(EVENTS)
        .col(text("exam_id"))
        .col(user_id())
        .col(sequence("seq", 1))
        .col(text("event"))
        .col(text("received_at"))
        .primary_key(Index::create().col("exam_id").col("oj_user_id").col("seq"))
        .foreign_key(&mut runtime_foreign_key(EVENTS))
        .to_owned()
}

fn incidents() -> TableCreateStatement {
    Table::create()
        .table(INCIDENTS)
        .col(text("incident_id").primary_key())
        .col(text("exam_id"))
        .col(user_id())
        .col(text("kind"))
        .col(text("occurred_at"))
        .col(ColumnDef::new("detail").text())
        .foreign_key(&mut runtime_foreign_key(INCIDENTS))
        .take()
}

fn audit() -> TableCreateStatement {
    Table::create()
        .table(AUDIT)
        .col(integer("audit_id").primary_key())
        .col(text("actor"))
        .col(text("operation"))
        .col(text("occurred_at"))
        .take()
}

fn indexes() -> [IndexCreateStatement; 6] {
    [
        Index::create()
            .name("exams_by_start")
            .table(EXAMS)
            .col("start_at")
            .col("exam_id")
            .take(),
        Index::create()
            .name("enrollments_by_user")
            .table(ENROLLMENTS)
            .col("oj_user_id")
            .col("exam_id")
            .take(),
        Index::create()
            .name("student_runtimes_online")
            .table(RUNTIMES)
            .col("exam_id")
            .col("oj_user_id")
            .and_where(Expr::col("connected").eq(1))
            .take(),
        incident_index("incidents_by_exam_time", None),
        incident_index("incidents_by_student_time", Some("oj_user_id")),
        incident_index("incidents_by_kind_time", Some("kind")),
    ]
}

fn incident_index(name: &str, filter: Option<&'static str>) -> IndexCreateStatement {
    let mut index = Index::create();
    index.name(name).table(INCIDENTS).col("exam_id");
    if let Some(field) = filter {
        index.col(field);
    }
    index.col("occurred_at").col("incident_id").take()
}

fn text(column: &'static str) -> ColumnDef {
    ColumnDef::new(column).text().not_null().to_owned()
}

fn integer(column: &'static str) -> ColumnDef {
    ColumnDef::new(column).integer().not_null().take()
}

fn user_id() -> ColumnDef {
    integer("oj_user_id")
        .check(Expr::col("oj_user_id").gt(0))
        .to_owned()
}

fn policy_value(column: &'static str) -> ColumnDef {
    integer(column)
        .check(Expr::col(column).between(1, i64::from(u32::MAX)))
        .to_owned()
}

fn sequence(column: &'static str, minimum: i64) -> ColumnDef {
    let value = Expr::col(column);
    let column_type = Expr::from(Func::cust("typeof").arg(value.clone()));
    integer(column)
        .check(column_type.eq("integer").and(value.gte(minimum)))
        .take()
}

fn exam_foreign_key(table: &'static str) -> ForeignKeyCreateStatement {
    ForeignKey::create()
        .from(table, "exam_id")
        .to(EXAMS, "exam_id")
        .to_owned()
}

fn runtime_foreign_key(table: &'static str) -> ForeignKeyCreateStatement {
    ForeignKey::create()
        .from(table, ("exam_id", "oj_user_id"))
        .to(RUNTIMES, ("exam_id", "oj_user_id"))
        .to_owned()
}
