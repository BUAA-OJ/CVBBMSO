use sea_orm_migration::prelude::*;

use super::initial::{
    self, AUDIT, COMMANDS, EVENTS, EXAMS, INCIDENTS, MANIFESTS, RUNTIMES, exam_foreign_key,
    runtime_foreign_key, text, user_id,
};

pub(super) struct Simplify;

const OLD_RUNTIME: &str = "migration_runtimes";
const OLD_COMMAND: &str = "migration_commands";
const OLD_EVENT: &str = "migration_events";
const OLD_INCIDENT: &str = "migration_incidents";
const OLD_AUDIT: &str = "migration_audit";
const RENAMED: [(&str, &str); 5] = [
    (RUNTIMES, OLD_RUNTIME),
    (COMMANDS, OLD_COMMAND),
    (EVENTS, OLD_EVENT),
    (INCIDENTS, OLD_INCIDENT),
    (AUDIT, OLD_AUDIT),
];

const STUDENT: [&str; 2] = ["exam_id", "oj_user_id"];
const RUNTIME_FIELDS: [&str; 8] = [
    "exam_id",
    "oj_user_id",
    "device_id",
    "instance_id",
    "connected",
    "execution",
    "recording",
    "last_seen_at",
];
const COMMAND_FIELDS: [&str; 4] = ["command_id", "exam_id", "oj_user_id", "name"];
const EVENT_FIELDS: [&str; 4] = ["exam_id", "oj_user_id", "event", "received_at"];
const INCIDENT_FIELDS: [&str; 6] = [
    "incident_id",
    "exam_id",
    "oj_user_id",
    "kind",
    "occurred_at",
    "detail",
];

impl MigrationName for Simplify {
    fn name(&self) -> &'static str {
        "m20261120_000002_simplify"
    }
}

#[sea_orm_migration::async_trait::async_trait]
impl MigrationTrait for Simplify {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (table, field) in [(RUNTIMES, "ack_event_through"), (EVENTS, "seq")] {
            let beyond_limit = Query::select()
                .column(field)
                .from(table)
                .and_where(Expr::col(field).gt(i64::MAX.to_be_bytes().to_vec()))
                .limit(1)
                .take();
            if manager
                .get_connection()
                .query_one(&beyond_limit)
                .await?
                .is_some()
            {
                return Err(DbErr::Migration(format!(
                    "{table}.{field} exceeds i64::MAX"
                )));
            }
        }

        rename_tables(manager).await?;
        for table in tables() {
            manager.create_table(table).await?;
        }
        copy_to(
            manager,
            RUNTIMES,
            RUNTIME_FIELDS.into_iter().chain(["ack_event_through"]),
            select(OLD_RUNTIME, RUNTIME_FIELDS).expr(decode_sequence("ack_event_through")),
        )
        .await?;
        copy_to(
            manager,
            COMMANDS,
            COMMAND_FIELDS.into_iter().chain(["result"]),
            select(OLD_COMMAND, COMMAND_FIELDS)
                .expr(
                    Expr::case(
                        Expr::col("result_error_code").is_null(),
                        json_object([("status", Expr::col("result_status"))]),
                    )
                    .finally(json_object([
                        ("status", Expr::col("result_status")),
                        ("error_code", Expr::col("result_error_code")),
                    ])),
                )
                .and_where(Expr::col("lifecycle").eq("reported"))
                .and_where(Expr::col("result_status").is_not_null()),
        )
        .await?;
        copy_to(
            manager,
            EVENTS,
            EVENT_FIELDS.into_iter().chain(["seq"]),
            select(OLD_EVENT, EVENT_FIELDS).expr(decode_sequence("seq")),
        )
        .await?;
        copy_to(
            manager,
            INCIDENTS,
            INCIDENT_FIELDS,
            &mut select(OLD_INCIDENT, INCIDENT_FIELDS),
        )
        .await?;
        upgrade_audit(manager).await?;

        // Break the old runtime/incident foreign key cycle before dropping either table.
        manager
            .execute(
                Query::update()
                    .table(OLD_RUNTIME)
                    .value("lost_incident_id", None::<String>)
                    .and_where(Expr::col("lost_incident_id").is_not_null())
                    .take(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(MANIFESTS).take())
            .await?;
        drop_renamed_tables(manager).await?;
        for index in indexes() {
            manager.create_index(index).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rename_tables(manager).await?;
        for table in [
            initial::runtimes(),
            initial::commands(),
            initial::events(),
            initial::incidents(),
            initial::manifests(),
            initial::audit(),
        ] {
            manager.create_table(table).await?;
        }
        copy_to(
            manager,
            RUNTIMES,
            RUNTIME_FIELDS
                .into_iter()
                .chain(["ack_event_through", "platform", "connection_id"]),
            select(OLD_RUNTIME, RUNTIME_FIELDS)
                .expr(encode_sequence("ack_event_through"))
                .expr(Expr::val("windows"))
                .column("instance_id"),
        )
        .await?;
        restore_commands(manager).await?;
        copy_to(
            manager,
            EVENTS,
            EVENT_FIELDS.into_iter().chain(["seq"]),
            select(OLD_EVENT, EVENT_FIELDS).expr(encode_sequence("seq")),
        )
        .await?;
        restore_incidents(manager).await?;
        restore_manifests(manager).await?;
        restore_audit(manager).await?;
        drop_renamed_tables(manager).await?;
        for index in initial::evidence_indexes() {
            manager.create_index(index).await?;
        }
        Ok(())
    }
}

fn tables() -> [TableCreateStatement; 5] {
    [
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
            .take(),
        Table::create()
            .table(COMMANDS)
            .col(text("command_id").primary_key())
            .col(text("exam_id"))
            .col(user_id())
            .col(text("name").check(Expr::col("name").is_in(["start_exam", "end_exam"])))
            .col(text("result"))
            .foreign_key(&mut runtime_foreign_key(COMMANDS))
            .take(),
        Table::create()
            .table(EVENTS)
            .col(text("exam_id"))
            .col(user_id())
            .col(sequence("seq", 1))
            .col(text("event"))
            .col(text("received_at"))
            .primary_key(Index::create().col("exam_id").col("oj_user_id").col("seq"))
            .foreign_key(&mut runtime_foreign_key(EVENTS))
            .take(),
        Table::create()
            .table(INCIDENTS)
            .col(text("incident_id").primary_key())
            .col(text("exam_id"))
            .col(user_id())
            .col(text("kind"))
            .col(text("occurred_at"))
            .col(ColumnDef::new("detail").text())
            .foreign_key(&mut runtime_foreign_key(INCIDENTS))
            .take(),
        Table::create()
            .table(AUDIT)
            .col(integer("audit_id").primary_key())
            .col(text("actor"))
            .col(text("operation"))
            .col(text("occurred_at"))
            .take(),
    ]
}

fn indexes() -> [IndexCreateStatement; 4] {
    [
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

fn integer(field: &'static str) -> ColumnDef {
    ColumnDef::new(field).integer().not_null().take()
}

fn sequence(field: &'static str, minimum: i64) -> ColumnDef {
    integer(field)
        .check(
            function("typeof", [Expr::col(field)])
                .eq("integer")
                .and(Expr::col(field).gte(minimum)),
        )
        .take()
}

async fn rename_tables(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for (from, to) in RENAMED {
        manager
            .rename_table(Table::rename().table(from, to).take())
            .await?;
    }
    Ok(())
}

async fn drop_renamed_tables(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for table in [OLD_AUDIT, OLD_INCIDENT, OLD_EVENT, OLD_COMMAND, OLD_RUNTIME] {
        manager
            .drop_table(Table::drop().table(table).take())
            .await?;
    }
    Ok(())
}

fn select(table: &'static str, columns: impl IntoIterator<Item = &'static str>) -> SelectStatement {
    Query::select().from(table).columns(columns).take()
}

async fn copy_to(
    manager: &SchemaManager<'_>,
    table: &'static str,
    columns: impl IntoIterator<Item = &'static str>,
    source: &mut SelectStatement,
) -> Result<(), DbErr> {
    let insert = Query::insert()
        .into_table(table)
        .columns(columns)
        .select_from(source.take())
        .map_err(|error| DbErr::Migration(error.to_string()))?
        .take();
    manager.execute(insert).await
}

fn function(name: &'static str, args: impl IntoIterator<Item = Expr>) -> Expr {
    Func::cust(name).args(args).into()
}

fn json_field(value: Expr, path: &'static str) -> Expr {
    function("json_extract", [value, Expr::val(path)])
}

fn json_object(entries: impl IntoIterator<Item = (&'static str, Expr)>) -> Expr {
    function(
        "json_object",
        entries
            .into_iter()
            .flat_map(|(key, value)| [Expr::val(key), value]),
    )
}

fn decode_sequence(column: &'static str) -> Expr {
    // SQLite cannot cast hex text to an integer. Decode the old big-endian blob
    // nibble by nibble after rejecting values whose high bit is set.
    let hex = function("hex", [Expr::col(column)]);
    (1..=16).fold(Expr::val(0), |value, offset| {
        let digit = function("substr", [hex.clone(), Expr::val(offset), Expr::val(1)]);
        let digit = function("instr", [Expr::val("0123456789ABCDEF"), digit]).sub(1);
        value.left_shift(4).add(digit)
    })
}

fn encode_sequence(column: &'static str) -> Expr {
    function(
        "unhex",
        [function("printf", [Expr::val("%016X"), Expr::col(column)])],
    )
}

async fn upgrade_audit(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let audit_column = |column| Expr::col((OLD_AUDIT, column));
    let exam_column = |column| Expr::col((EXAMS, column));
    let operation = audit_column("operation");
    let targets = audit_column("target_users");
    let operation_json = |details: &[(&'static str, Expr)]| {
        json_object(
            [
                ("operation", operation.clone()),
                ("exam_id", audit_column("exam_id")),
            ]
            .into_iter()
            .chain(details.iter().cloned()),
        )
    };

    let mut source = Query::select();
    source
        .columns([(OLD_AUDIT, "actor"), (OLD_AUDIT, "occurred_at")])
        .expr(
            Expr::case(
                operation.clone().eq("exams_create"),
                operation_json(&[
                    ("title", exam_column("title")),
                    ("start_at", exam_column("start_at")),
                    ("end_at", exam_column("end_at")),
                    (
                        "policy",
                        json_object([
                            (
                                "segment_duration_secs",
                                exam_column("segment_duration_secs"),
                            ),
                            ("fps", exam_column("fps")),
                        ]),
                    ),
                ]),
            )
            .case(
                operation.clone().eq("participants_import"),
                operation_json(&[
                    ("oj_user_ids", function("json", [targets.clone()])),
                    ("change_count", audit_column("change_count")),
                ]),
            )
            .finally(operation_json(&[(
                "oj_user_id",
                json_field(targets, "$[0]"),
            )])),
        )
        .from(OLD_AUDIT)
        .inner_join(EXAMS, audit_column("exam_id").eq(exam_column("exam_id")))
        .order_by((OLD_AUDIT, "occurred_at"), Order::Asc)
        .order_by((OLD_AUDIT, "audit_id"), Order::Asc);
    copy_to(
        manager,
        AUDIT,
        ["actor", "occurred_at", "operation"],
        &mut source,
    )
    .await
}

async fn restore_commands(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let effective_at: Expr = Expr::case(
        Expr::col((OLD_COMMAND, "name")).eq("start_exam"),
        Expr::col((EXAMS, "start_at")),
    )
    .finally(Expr::col((EXAMS, "end_at")))
    .into();
    let mut source = Query::select();
    source
        .columns(COMMAND_FIELDS.map(|column| (OLD_COMMAND, column)))
        .column((RUNTIMES, "connection_id"))
        .expr(effective_at.clone())
        .expr(effective_at)
        .expr(Expr::val("reported"))
        .expr(json_field(Expr::col("result"), "$.status"))
        .expr(json_field(Expr::col("result"), "$.error_code"))
        .from(OLD_COMMAND)
        .inner_join(
            EXAMS,
            Expr::col((OLD_COMMAND, "exam_id")).equals((EXAMS, "exam_id")),
        )
        .inner_join(RUNTIMES, same_student(OLD_COMMAND, RUNTIMES));
    copy_to(
        manager,
        COMMANDS,
        COMMAND_FIELDS.into_iter().chain([
            "connection_id",
            "effective_at",
            "created_at",
            "lifecycle",
            "result_status",
            "result_error_code",
        ]),
        &mut source,
    )
    .await
}

fn same_student(first: &'static str, second: &'static str) -> Expr {
    Expr::col((first, "exam_id"))
        .equals((second, "exam_id"))
        .and(Expr::col((first, "oj_user_id")).equals((second, "oj_user_id")))
}

async fn restore_incidents(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    copy_to(
        manager,
        INCIDENTS,
        INCIDENT_FIELDS.into_iter().chain(["source"]),
        select(OLD_INCIDENT, INCIDENT_FIELDS)
            .expr(
                Expr::case(
                    Expr::col("kind").is_in([
                        "connection_lost",
                        "connection_restored",
                        "process_restarted",
                    ]),
                    "server",
                )
                .finally("client"),
            )
            .order_by("occurred_at", Order::Asc)
            .order_by("incident_id", Order::Asc),
    )
    .await?;
    let last_loss = select(INCIDENTS, ["incident_id"])
        .and_where(same_student(INCIDENTS, RUNTIMES))
        .and_where(Expr::col("kind").eq("connection_lost"))
        .order_by("occurred_at", Order::Desc)
        .order_by("incident_id", Order::Desc)
        .limit(1)
        .take();
    manager
        .execute(
            Query::update()
                .table(RUNTIMES)
                .value("lost_incident_id", Expr::from(last_loss))
                .and_where(Expr::col("connected").eq(0))
                .and_where(Expr::col("execution").ne("finished"))
                .take(),
        )
        .await
}

async fn restore_manifests(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    // Rebuild the initial migration's derived view using the legacy selection rule.
    let manifests = select(EVENTS, STUDENT)
        .expr_as(Func::max(Expr::col("seq")), "seq")
        .and_where(json_field(Expr::col("event"), "$.kind").eq("recording_finalized"))
        .group_by_columns(STUDENT)
        .take();
    let mut source = Query::select();
    source
        .columns(["exam_id", "oj_user_id", "seq"].map(|column| (EVENTS, column)))
        .expr(json_field(Expr::col((EVENTS, "event")), "$.data"))
        .from(EVENTS)
        .join_subquery(
            JoinType::InnerJoin,
            manifests,
            "latest",
            same_student(EVENTS, "latest")
                .and(Expr::col((EVENTS, "seq")).equals(("latest", "seq"))),
        );
    copy_to(
        manager,
        MANIFESTS,
        ["exam_id", "oj_user_id", "source_seq", "manifest"],
        &mut source,
    )
    .await
}

async fn restore_audit(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let field = |path| json_field(Expr::col("operation"), path);
    let operation = field("$.operation");
    let mut source = select(OLD_AUDIT, ["actor", "occurred_at"]);
    source
        .expr(function(
            "printf",
            [
                Expr::val("00000000-0000-0000-%04x-%012x"),
                Expr::col("audit_id").right_shift(48),
                Expr::col("audit_id").bit_and(0x0000_ffff_ffff_ffff_i64),
            ],
        ))
        .expr(operation.clone())
        .expr(field("$.exam_id"))
        .expr(
            Expr::case(
                operation.clone().eq("participants_import"),
                field("$.oj_user_ids"),
            )
            .case(
                operation
                    .clone()
                    .is_in(["participants_add", "participants_remove"]),
                function("json_array", [field("$.oj_user_id")]),
            )
            .finally(Expr::val(None::<String>)),
        )
        .expr(
            Expr::case(
                operation.eq("participants_import"),
                field("$.change_count")
                    .if_null(function("json_array_length", [field("$.oj_user_ids")])),
            )
            .finally(1),
        );
    copy_to(
        manager,
        AUDIT,
        [
            "actor",
            "occurred_at",
            "audit_id",
            "operation",
            "exam_id",
            "target_users",
            "change_count",
        ],
        &mut source,
    )
    .await
}
