use cvbbmso_protocol::AccodingUser;
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbBackend, FromQueryResult, Statement};
use uuid::Uuid;

use crate::{DomainError, Service, db, student_registration};

/// Display metadata for a student with recorded exam activity.
#[derive(Debug, FromQueryResult)]
pub struct StudentIdentity {
    /// The Accoding account used to correlate evidence.
    pub oj_user_id: i64,
    /// The exam's bound school number, or the latest verified campus login number.
    pub student_number: Option<String>,
    /// The latest usable Accoding display name, if observed.
    pub nickname: Option<String>,
}

pub(crate) async fn remember(
    service: &Service,
    user: &AccodingUser,
    has_current_eligibility: bool,
) -> Result<(), DomainError> {
    db::validate_user(user.id)?;
    if !has_current_eligibility
        && service
            .reader
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT 1 FROM student_runtimes WHERE oj_user_id = ? LIMIT 1",
                [user.id.into()],
            ))
            .await?
            .is_none()
    {
        return Ok(());
    }
    let number = student_registration::school_number(&user.username);
    let name = user.nickname.trim();
    let nickname = (!name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control))
        .then_some(name);
    let values = || [user.id.into(), number.into(), nickname.into()];
    // A list refresh or socket reconnect usually repeats the same data. Avoid
    // taking the single SQLite writer connection in that case.
    let existing = service
        .reader
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT 1 FROM student_profiles
             WHERE oj_user_id = ?1 AND student_number IS ?2 AND nickname IS ?3",
            values(),
        ))
        .await?;
    if existing.is_none() {
        service
            .writer
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO student_profiles (oj_user_id, student_number, nickname)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (oj_user_id) DO UPDATE SET
                    student_number = excluded.student_number, nickname = excluded.nickname
                 WHERE student_profiles.student_number IS NOT excluded.student_number
                    OR student_profiles.nickname IS NOT excluded.nickname",
                values(),
            ))
            .await?;
    }
    Ok(())
}

pub(crate) async fn list(
    tx: &DatabaseTransaction,
    exam_id: Uuid,
) -> Result<Vec<StudentIdentity>, DomainError> {
    let id = exam_id.to_string();
    db::exam(tx, &id).await?;
    Ok(StudentIdentity::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT rt.oj_user_id, COALESCE(binding.student_number, profile.student_number) AS student_number,
            profile.nickname
         FROM student_runtimes AS rt
         LEFT JOIN student_enrollments AS binding
           ON binding.exam_id = rt.exam_id AND binding.oj_user_id = rt.oj_user_id
         LEFT JOIN student_profiles AS profile ON profile.oj_user_id = rt.oj_user_id
         WHERE rt.exam_id = ? ORDER BY rt.oj_user_id",
        [id.into()],
    ))
    .all(tx)
    .await?)
}
