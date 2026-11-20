use sea_orm_migration::prelude::*;

pub(super) struct Initial;

// Keep this schema independent of entities that may change in future migrations.
pub(super) const EXAMS: &str = "exams";
const ENROLLMENTS: &str = "enrollments";
pub(super) const RUNTIMES: &str = "student_runtimes";
pub(super) const COMMANDS: &str = "commands";
pub(super) const EVENTS: &str = "events";
pub(super) const INCIDENTS: &str = "incidents";
pub(super) const MANIFESTS: &str = "recording_manifests";
pub(super) const AUDIT: &str = "admin_audit";

impl MigrationName for Initial {
    fn name(&self) -> &'static str {
        "m20261119_000001_initial"
    }
}

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Initial {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for table in tables() {
            manager.create_table(table).await?;
        }
        for index in indexes() {
            manager.create_index(index).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Break the runtime/incident reference cycle before dropping either table.
        manager
            .execute(
                Query::update()
                    .table(RUNTIMES)
                    .value("lost_incident_id", None::<String>)
                    .and_where(Expr::col("lost_incident_id").is_not_null())
                    .to_owned(),
            )
            .await?;
        for table in [
            AUDIT,
            MANIFESTS,
            INCIDENTS,
            EVENTS,
            COMMANDS,
            RUNTIMES,
            ENROLLMENTS,
            EXAMS,
        ] {
            manager
                .drop_table(Table::drop().table(table).to_owned())
                .await?;
        }
        Ok(())
    }
}

fn tables() -> [TableCreateStatement; 8] {
    [
        exams(),
        enrollments(),
        runtimes(),
        commands(),
        events(),
        incidents(),
        manifests(),
        audit(),
    ]
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

pub(super) fn runtimes() -> TableCreateStatement {
    Table::create()
        .table(RUNTIMES)
        .col(text("exam_id"))
        .col(user_id())
        .col(text("device_id"))
        .col(text("instance_id"))
        .col(text("platform").check(Expr::col("platform").is_in(["windows", "macos"])))
        .col(text("connection_id"))
        .col(
            ColumnDef::new("connected")
                .integer()
                .not_null()
                .check(Expr::col("connected").is_in([0, 1])),
        )
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
        .col(
            ColumnDef::new("ack_event_through")
                .blob()
                .not_null()
                .check(sequence_blob("ack_event_through")),
        )
        .col(ColumnDef::new("lost_incident_id").text())
        .primary_key(Index::create().col("exam_id").col("oj_user_id"))
        .foreign_key(&mut exam_foreign_key(RUNTIMES))
        .foreign_key(
            ForeignKey::create()
                .from(RUNTIMES, "lost_incident_id")
                .to(INCIDENTS, "incident_id"),
        )
        .to_owned()
}

pub(super) fn commands() -> TableCreateStatement {
    Table::create()
        .table(COMMANDS)
        .col(text("command_id").primary_key())
        .col(text("exam_id"))
        .col(user_id())
        .col(text("connection_id"))
        .col(text("name").check(Expr::col("name").is_in(["start_exam", "end_exam"])))
        .col(text("effective_at"))
        .col(text("created_at"))
        .col(text("lifecycle").check(Expr::col("lifecycle").is_in([
            "pending",
            "reported",
            "inferred",
            "superseded",
        ])))
        .col(
            ColumnDef::new("result_status")
                .text()
                .check(Expr::col("result_status").is_in(["completed", "failed"])),
        )
        .col(ColumnDef::new("result_error_code").text())
        .foreign_key(&mut runtime_foreign_key(COMMANDS))
        .to_owned()
}

pub(super) fn events() -> TableCreateStatement {
    Table::create()
        .table(EVENTS)
        .col(text("exam_id"))
        .col(user_id())
        .col(
            ColumnDef::new("seq")
                .blob()
                .not_null()
                .check(sequence_blob("seq").and(Expr::col("seq").gt(Expr::val(vec![0_u8; 8])))),
        )
        .col(text("event"))
        .col(text("received_at"))
        .primary_key(Index::create().col("exam_id").col("oj_user_id").col("seq"))
        .foreign_key(&mut runtime_foreign_key(EVENTS))
        .to_owned()
}

pub(super) fn incidents() -> TableCreateStatement {
    Table::create()
        .table(INCIDENTS)
        .col(ColumnDef::new("ordinal").integer().primary_key())
        .col(text("incident_id").unique_key())
        .col(text("exam_id"))
        .col(user_id())
        .col(text("source").check(Expr::col("source").is_in(["server", "client"])))
        .col(text("kind"))
        .col(text("occurred_at"))
        .col(ColumnDef::new("detail").text())
        .col(ColumnDef::new("event_seq").blob())
        .foreign_key(&mut runtime_foreign_key(INCIDENTS))
        .foreign_key(&mut event_foreign_key(INCIDENTS, "event_seq"))
        .to_owned()
}

pub(super) fn manifests() -> TableCreateStatement {
    Table::create()
        .table(MANIFESTS)
        .col(text("exam_id"))
        .col(user_id())
        .col(ColumnDef::new("source_seq").blob().not_null())
        .col(text("manifest"))
        .primary_key(Index::create().col("exam_id").col("oj_user_id"))
        .foreign_key(&mut runtime_foreign_key(MANIFESTS))
        .foreign_key(&mut event_foreign_key(MANIFESTS, "source_seq"))
        .to_owned()
}

pub(super) fn audit() -> TableCreateStatement {
    Table::create()
        .table(AUDIT)
        .col(text("audit_id").primary_key())
        .col(text("actor"))
        .col(text("operation"))
        .col(text("exam_id"))
        .col(ColumnDef::new("target_users").text())
        .col(
            ColumnDef::new("change_count")
                .integer()
                .not_null()
                .check(Expr::col("change_count").gte(0)),
        )
        .col(text("occurred_at"))
        .foreign_key(&mut exam_foreign_key(AUDIT))
        .to_owned()
}

fn indexes() -> impl Iterator<Item = IndexCreateStatement> {
    [
        Index::create()
            .name("exams_by_start")
            .table(EXAMS)
            .col("start_at")
            .col("exam_id")
            .to_owned(),
        Index::create()
            .name("enrollments_by_user")
            .table(ENROLLMENTS)
            .col("oj_user_id")
            .col("exam_id")
            .to_owned(),
    ]
    .into_iter()
    .chain(evidence_indexes())
}

pub(super) fn evidence_indexes() -> [IndexCreateStatement; 7] {
    [
        Index::create()
            .name("student_runtimes_online")
            .table(RUNTIMES)
            .col("exam_id")
            .col("oj_user_id")
            .and_where(Expr::col("connected").eq(1))
            .to_owned(),
        Index::create()
            .name("pending_commands_by_connection")
            .table(COMMANDS)
            .col("exam_id")
            .col("oj_user_id")
            .col("connection_id")
            .col("name")
            .unique()
            .and_where(Expr::col("lifecycle").eq("pending"))
            .to_owned(),
        Index::create()
            .name("incident_by_event")
            .table(INCIDENTS)
            .col("exam_id")
            .col("oj_user_id")
            .col("event_seq")
            .unique()
            .and_where(Expr::col("event_seq").is_not_null())
            .to_owned(),
        Index::create()
            .name("incidents_by_exam_time")
            .table(INCIDENTS)
            .col("exam_id")
            .col("occurred_at")
            .col("ordinal")
            .to_owned(),
        Index::create()
            .name("incidents_by_student_time")
            .table(INCIDENTS)
            .col("exam_id")
            .col("oj_user_id")
            .col("occurred_at")
            .col("ordinal")
            .to_owned(),
        Index::create()
            .name("incidents_by_kind_time")
            .table(INCIDENTS)
            .col("exam_id")
            .col("kind")
            .col("occurred_at")
            .col("ordinal")
            .to_owned(),
        Index::create()
            .name("admin_audit_by_exam")
            .table(AUDIT)
            .col("exam_id")
            .col("occurred_at")
            .col("audit_id")
            .to_owned(),
    ]
}

pub(super) fn text(column: &'static str) -> ColumnDef {
    ColumnDef::new(column).text().not_null().to_owned()
}

pub(super) fn user_id() -> ColumnDef {
    ColumnDef::new("oj_user_id")
        .integer()
        .not_null()
        .check(Expr::col("oj_user_id").gt(0))
        .to_owned()
}

fn policy_value(column: &'static str) -> ColumnDef {
    ColumnDef::new(column)
        .integer()
        .not_null()
        .check(Expr::col(column).between(1, i64::from(u32::MAX)))
        .to_owned()
}

fn sequence_blob(column: &'static str) -> Expr {
    Expr::from(Func::cust("typeof").arg(Expr::col(column)))
        .eq("blob")
        .and(Expr::from(Func::char_length(Expr::col(column))).eq(8))
}

pub(super) fn exam_foreign_key(table: &'static str) -> ForeignKeyCreateStatement {
    ForeignKey::create()
        .from(table, "exam_id")
        .to(EXAMS, "exam_id")
        .to_owned()
}

pub(super) fn runtime_foreign_key(table: &'static str) -> ForeignKeyCreateStatement {
    ForeignKey::create()
        .from(table, ("exam_id", "oj_user_id"))
        .to(RUNTIMES, ("exam_id", "oj_user_id"))
        .to_owned()
}

fn event_foreign_key(table: &'static str, sequence: &'static str) -> ForeignKeyCreateStatement {
    ForeignKey::create()
        .from(table, ("exam_id", "oj_user_id", sequence))
        .to(EVENTS, ("exam_id", "oj_user_id", "seq"))
        .to_owned()
}
