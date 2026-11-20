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

#[derive(Clone, Copy)]
pub(super) enum Schema {
    Initial,
    Simplified,
}

impl MigrationName for Initial {
    fn name(&self) -> &'static str {
        "m20261119_000001_initial"
    }
}

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Initial {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for table in [exams(), enrollments()]
            .into_iter()
            .chain(Schema::Initial.evidence_tables())
        {
            manager.create_table(table).await?;
        }
        for index in indexes() {
            manager.create_index(index).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Break the runtime/incident reference cycle before dropping either table.
        clear_lost_incidents(manager, RUNTIMES).await?;
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

pub(super) async fn clear_lost_incidents(
    manager: &SchemaManager<'_>,
    table: &'static str,
) -> Result<(), DbErr> {
    manager
        .execute(
            Query::update()
                .table(table)
                .value("lost_incident_id", None::<String>)
                .and_where(Expr::col("lost_incident_id").is_not_null())
                .take(),
        )
        .await
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

impl Schema {
    pub(super) fn evidence_tables(self) -> Vec<TableCreateStatement> {
        let mut tables = vec![
            runtimes(self),
            commands(self),
            events(self),
            incidents(self),
        ];
        if matches!(self, Self::Initial) {
            tables.push(manifests());
        }
        tables.push(audit(self));
        tables
    }

    fn sequence(self, column: &'static str, minimum: i64) -> ColumnDef {
        let value = Expr::col(column);
        match self {
            Self::Initial => {
                let mut check = column_type(column, "blob")
                    .and(Expr::from(Func::char_length(value.clone())).eq(8));
                if minimum == 1 {
                    check = check.and(value.gt(vec![0_u8; 8]));
                }
                ColumnDef::new(column).blob().not_null().check(check).take()
            }
            Self::Simplified => integer(column)
                .check(column_type(column, "integer").and(value.gte(minimum)))
                .take(),
        }
    }
}

fn runtimes(schema: Schema) -> TableCreateStatement {
    let mut table = Table::create();
    table
        .table(RUNTIMES)
        .col(text("exam_id"))
        .col(user_id())
        .col(text("device_id"))
        .col(text("instance_id"));
    if matches!(schema, Schema::Initial) {
        table
            .col(text("platform").check(Expr::col("platform").is_in(["windows", "macos"])))
            .col(text("connection_id"));
    }
    table
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
        .col(schema.sequence("ack_event_through", 0))
        .primary_key(Index::create().col("exam_id").col("oj_user_id"))
        .foreign_key(&mut exam_foreign_key(RUNTIMES));
    if matches!(schema, Schema::Initial) {
        table
            .col(ColumnDef::new("lost_incident_id").text())
            .foreign_key(
                ForeignKey::create()
                    .from(RUNTIMES, "lost_incident_id")
                    .to(INCIDENTS, "incident_id"),
            );
    }
    table.take()
}

fn commands(schema: Schema) -> TableCreateStatement {
    let mut table = Table::create();
    table
        .table(COMMANDS)
        .col(text("command_id").primary_key())
        .col(text("exam_id"))
        .col(user_id());
    if matches!(schema, Schema::Initial) {
        table.col(text("connection_id"));
    }
    table.col(text("name").check(Expr::col("name").is_in(["start_exam", "end_exam"])));
    match schema {
        Schema::Initial => table
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
            .col(ColumnDef::new("result_error_code").text()),
        Schema::Simplified => table.col(text("result")),
    }
    .foreign_key(&mut runtime_foreign_key(COMMANDS))
    .take()
}

fn events(schema: Schema) -> TableCreateStatement {
    Table::create()
        .table(EVENTS)
        .col(text("exam_id"))
        .col(user_id())
        .col(schema.sequence("seq", 1))
        .col(text("event"))
        .col(text("received_at"))
        .primary_key(Index::create().col("exam_id").col("oj_user_id").col("seq"))
        .foreign_key(&mut runtime_foreign_key(EVENTS))
        .to_owned()
}

fn incidents(schema: Schema) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(INCIDENTS);
    match schema {
        Schema::Initial => table
            .col(ColumnDef::new("ordinal").integer().primary_key())
            .col(text("incident_id").unique_key()),
        Schema::Simplified => table.col(text("incident_id").primary_key()),
    }
    .col(text("exam_id"))
    .col(user_id());
    if matches!(schema, Schema::Initial) {
        table.col(text("source").check(Expr::col("source").is_in(["server", "client"])));
    }
    table
        .col(text("kind"))
        .col(text("occurred_at"))
        .col(ColumnDef::new("detail").text())
        .foreign_key(&mut runtime_foreign_key(INCIDENTS));
    if matches!(schema, Schema::Initial) {
        table
            .col(ColumnDef::new("event_seq").blob())
            .foreign_key(&mut event_foreign_key(INCIDENTS, "event_seq"));
    }
    table.take()
}

fn manifests() -> TableCreateStatement {
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

fn audit(schema: Schema) -> TableCreateStatement {
    let mut table = Table::create();
    let mut id = match schema {
        Schema::Initial => text("audit_id"),
        Schema::Simplified => integer("audit_id"),
    };
    table
        .table(AUDIT)
        .col(id.primary_key())
        .col(text("actor"))
        .col(text("operation"));
    if matches!(schema, Schema::Initial) {
        table
            .col(text("exam_id"))
            .col(ColumnDef::new("target_users").text())
            .col(integer("change_count").check(Expr::col("change_count").gte(0)))
            .foreign_key(&mut exam_foreign_key(AUDIT));
    }
    table.col(text("occurred_at")).take()
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
    .chain(Schema::Initial.evidence_indexes())
}

impl Schema {
    pub(super) fn evidence_indexes(self) -> Vec<IndexCreateStatement> {
        let order = match self {
            Self::Initial => "ordinal",
            Self::Simplified => "incident_id",
        };
        let online = Index::create()
            .name("student_runtimes_online")
            .table(RUNTIMES)
            .col("exam_id")
            .col("oj_user_id")
            .and_where(Expr::col("connected").eq(1))
            .take();
        let mut indexes = vec![online];
        if matches!(self, Self::Initial) {
            indexes.extend(legacy_indexes());
        }
        indexes.extend([
            incident_index("incidents_by_exam_time", None, order),
            incident_index("incidents_by_student_time", Some("oj_user_id"), order),
            incident_index("incidents_by_kind_time", Some("kind"), order),
        ]);
        indexes
    }
}

fn legacy_indexes() -> [IndexCreateStatement; 3] {
    [
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
            .name("admin_audit_by_exam")
            .table(AUDIT)
            .col("exam_id")
            .col("occurred_at")
            .col("audit_id")
            .take(),
    ]
}

fn incident_index(
    name: &str,
    filter: Option<&'static str>,
    order: &'static str,
) -> IndexCreateStatement {
    let mut index = Index::create();
    index.name(name).table(INCIDENTS).col("exam_id");
    if let Some(field) = filter {
        index.col(field);
    }
    index.col("occurred_at").col(order).take()
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

fn column_type(column: &'static str, kind: &'static str) -> Expr {
    Expr::from(Func::cust("typeof").arg(Expr::col(column))).eq(kind)
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

fn event_foreign_key(table: &'static str, sequence: &'static str) -> ForeignKeyCreateStatement {
    ForeignKey::create()
        .from(table, ("exam_id", "oj_user_id", sequence))
        .to(EVENTS, ("exam_id", "oj_user_id", "seq"))
        .to_owned()
}
