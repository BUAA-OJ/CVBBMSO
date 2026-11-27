use std::collections::BTreeSet;

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, FromQueryResult,
    QueryFilter, QueryOrder, Set, Statement,
    sea_query::{Expr, ExprTrait, OnConflict, Query},
};
use uuid::Uuid;

use crate::{
    DomainError,
    db::{self, StoredExam, StudentKey},
    entity::{enrollment, exam, student_enrollment as roster},
};

/// A school student number on an exam roster.
#[derive(Debug)]
pub struct StudentEnrollment {
    /// Decimal school student number. Leading zeros are meaningful.
    pub student_number: String,
    /// Accoding user ID assigned on the first admitted connection.
    pub oj_user_id: Option<i64>,
    /// Whether the invitation is still active.
    pub enrolled: bool,
}

fn valid(number: &str) -> bool {
    (1..=20).contains(&number.len()) && number.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn school_number(username: &str) -> Option<&str> {
    let (number, domain) = username.split_once('@')?;
    (domain.eq_ignore_ascii_case("buaa.edu.cn") && valid(number)).then_some(number)
}

// A prior binding rejects changes on either side of the identity pair. Numeric
// admission remains available for accounts without a conflicting school binding.
const ELIGIBLE: &str = "
    NOT EXISTS (
        SELECT 1 FROM student_enrollments AS binding
        WHERE binding.exam_id = exams.exam_id
            AND (
                (binding.oj_user_id = ?1 AND (?2 IS NULL OR binding.student_number <> ?2))
                OR (binding.student_number = ?2 AND binding.oj_user_id <> ?1)
            )
    )
    AND (
        EXISTS (
            SELECT 1 FROM enrollments
            WHERE enrollments.exam_id = exams.exam_id AND enrollments.oj_user_id = ?1
        )
        OR EXISTS (
            SELECT 1 FROM student_enrollments AS invitation
            WHERE invitation.exam_id = exams.exam_id AND invitation.student_number = ?2
                AND invitation.enrolled = 1
                AND (invitation.oj_user_id IS NULL OR invitation.oj_user_id = ?1)
        )
    )";

pub(crate) async fn list_for(
    conn: &impl ConnectionTrait,
    oj_user_id: i64,
    number: Option<&str>,
) -> Result<Vec<StoredExam>, DomainError> {
    Ok(
        exam::Model::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!("SELECT * FROM exams WHERE {ELIGIBLE} ORDER BY start_at, exam_id"),
            [oj_user_id.into(), number.into()],
        ))
        .all(conn)
        .await?,
    )
}

pub(crate) async fn check(
    conn: &impl ConnectionTrait,
    key: &StudentKey,
    number: Option<&str>,
) -> Result<(), DomainError> {
    db::exam(conn, &key.exam).await?;
    let eligible = conn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!("SELECT 1 FROM exams WHERE exam_id = ?3 AND {ELIGIBLE}"),
            [key.user.into(), number.into(), key.exam.as_str().into()],
        ))
        .await?;
    eligible.ok_or(DomainError::NotEnrolled)?;
    Ok(())
}

pub(crate) async fn bind(
    tx: &DatabaseTransaction,
    key: &StudentKey,
    number: Option<&str>,
) -> Result<(), DomainError> {
    check(tx, key, number).await?;
    if let Some(number) = number {
        let changed = roster::Entity::update_many()
            .col_expr(roster::Column::OjUserId, Expr::val(key.user))
            .filter(roster::Column::ExamId.eq(&key.exam))
            .filter(roster::Column::StudentNumber.eq(number))
            .filter(roster::Column::Enrolled.eq(true))
            .filter(roster::Column::OjUserId.is_null())
            .exec(tx)
            .await?;
        if changed.rows_affected > 0 {
            let insert = Query::insert()
                .into_table(enrollment::Entity)
                .columns([enrollment::Column::ExamId, enrollment::Column::OjUserId])
                .values_panic([Expr::val(key.exam.as_str()), Expr::val(key.user)])
                .on_conflict(
                    OnConflict::columns([enrollment::Column::ExamId, enrollment::Column::OjUserId])
                        .do_nothing()
                        .to_owned(),
                )
                .to_owned();
            tx.execute_raw(DbBackend::Sqlite.build(&insert)).await?;
        }
    }
    Ok(())
}

pub(crate) async fn import(
    tx: &DatabaseTransaction,
    exam_id: Uuid,
    numbers: &[String],
) -> Result<usize, DomainError> {
    if !(1..=1_000).contains(&numbers.len()) || numbers.iter().any(|number| !valid(number)) {
        return Err(DomainError::InvalidInput);
    }
    let exam_id = exam_id.to_string();
    db::exam(tx, &exam_id).await?;
    let unique: Vec<_> = numbers
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut count = 0;
    for chunk in unique.chunks(300) {
        let mut insert = Query::insert();
        insert
            .into_table(roster::Entity)
            .columns([
                roster::Column::ExamId,
                roster::Column::StudentNumber,
                roster::Column::Enrolled,
            ])
            .on_conflict(
                OnConflict::columns([roster::Column::ExamId, roster::Column::StudentNumber])
                    .update_column(roster::Column::Enrolled)
                    .action_and_where(Expr::col(roster::Column::Enrolled).eq(false))
                    .to_owned(),
            );
        for number in chunk {
            insert.values_panic([
                Expr::val(exam_id.as_str()),
                Expr::val(*number),
                Expr::val(true),
            ]);
        }
        count += usize::try_from(
            tx.execute_raw(DbBackend::Sqlite.build(&insert))
                .await?
                .rows_affected(),
        )
        .map_err(DomainError::storage)?;
    }
    // Reimporting a revoked school registration reactivates the same verified
    // user. The prior binding survives revocation and cannot be reassigned.
    tx.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO enrollments (exam_id, oj_user_id)
         SELECT exam_id, oj_user_id FROM student_enrollments
         WHERE exam_id = ? AND enrolled = 1 AND oj_user_id IS NOT NULL
         ON CONFLICT (exam_id, oj_user_id) DO NOTHING",
        [exam_id.into()],
    ))
    .await?;
    Ok(count)
}

pub(crate) async fn list(
    tx: &DatabaseTransaction,
    exam_id: Uuid,
) -> Result<Vec<StudentEnrollment>, DomainError> {
    let exam_id = exam_id.to_string();
    db::exam(tx, &exam_id).await?;
    Ok(roster::Entity::find()
        .filter(roster::Column::ExamId.eq(exam_id))
        .order_by_asc(roster::Column::StudentNumber)
        .all(tx)
        .await?
        .into_iter()
        .map(|row| StudentEnrollment {
            student_number: row.student_number,
            oj_user_id: row.oj_user_id,
            enrolled: row.enrolled,
        })
        .collect())
}

pub(crate) async fn revoke_pending(
    tx: &DatabaseTransaction,
    exam_id: String,
    number: &str,
) -> Result<(), DomainError> {
    if !valid(number) {
        return Err(DomainError::InvalidInput);
    }
    let registration = roster::Entity::find_by_id((exam_id, number.to_owned()))
        .one(tx)
        .await?
        .filter(|row| row.enrolled)
        .ok_or(DomainError::NotFound)?;
    if registration.oj_user_id.is_some() {
        return Err(DomainError::Conflict);
    }
    let mut registration: roster::ActiveModel = registration.into();
    registration.enrolled = Set(false);
    roster::Entity::update(registration).exec(tx).await?;
    Ok(())
}

pub(crate) async fn revoke_bound(
    tx: &DatabaseTransaction,
    key: &StudentKey,
) -> Result<(), DomainError> {
    roster::Entity::update_many()
        .col_expr(roster::Column::Enrolled, Expr::val(false))
        .filter(roster::Column::ExamId.eq(&key.exam))
        .filter(roster::Column::OjUserId.eq(key.user))
        .filter(roster::Column::Enrolled.eq(true))
        .exec(tx)
        .await?;
    Ok(())
}
