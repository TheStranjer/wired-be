//! The moderators' ledger, `report_notes`: [`create_note`] writes a note,
//! [`notes_for`] and [`notes_for_reports`] read them back oldest first, and
//! [`delete_note`] removes one by its `(report, note)` pair. No client
//! endpoint touches this table; it is the admin API's own.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

use super::ReportNote;

/// [`create_note`]: the minted id (decision 4), the report, the moderator
/// who wrote it and the text; `inserted_at` is the column default's and
/// `RETURNING *` brings the stored row back.
const INSERT_NOTE_SQL: &str = "INSERT INTO report_notes (id, report_id, user_id, content)
     VALUES ($1, $2, $3, $4) RETURNING *";

/// [`notes_for_reports`]: the notes of many reports at once, each report's
/// own notes oldest first (`UUIDv7` text sorts in creation order, decision
/// 4 — the order `report_notes_report_id_idx` states). An empty id list is
/// answered by skipping the read.
const NOTES_FOR_SQL: &str = "SELECT * FROM report_notes
     WHERE report_id = ANY($1) ORDER BY id";

/// [`delete_note`]: the one writer, keyed on the pair so a note of another
/// report is as absent here as it is to a caller that never saw it;
/// `RETURNING *` hands the deleted row back to the moderation-log entry
/// that names its text.
const DELETE_NOTE_SQL: &str = "DELETE FROM report_notes
     WHERE id = $1 AND report_id = $2 RETURNING *";

/// Writes `user`'s note on the report `report_id` and returns the stored
/// row.
///
/// Notes are staff-only (Pleroma's `ReportNote.create` is reachable only
/// through the admin API) and each writing is its own row; nothing here
/// caps the text — the endpoint's validation does.
///
/// # Errors
///
/// Any database failure (connection, encoding, a report or author that
/// names no row) arrives as [`AppError::Database`](crate::error::AppError);
/// the foreign keys are the model's guards.
pub async fn create_note(
    pool: &PgPool,
    report_id: Uuid,
    user_id: Uuid,
    content: &str,
) -> AppResult<ReportNote> {
    let row = sqlx::query_as::<_, ReportNote>(INSERT_NOTE_SQL)
        .bind(new_id())
        .bind(report_id)
        .bind(user_id)
        .bind(content)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The notes on the report `report_id`, oldest first — the order
/// `report_notes_report_id_idx` states and the entity answers them in.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn notes_for(pool: &PgPool, report_id: Uuid) -> AppResult<Vec<ReportNote>> {
    notes_for_reports(pool, std::slice::from_ref(&report_id)).await
}

/// The notes on every report in `report_ids`, oldest first within each
/// report: [`notes_for`] for a whole page in one read, so the listing
/// renders every entity's `notes` array without a query per row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn notes_for_reports(pool: &PgPool, report_ids: &[Uuid]) -> AppResult<Vec<ReportNote>> {
    if report_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, ReportNote>(NOTES_FOR_SQL)
        .bind(report_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Deletes the note `note_id` of the report `report_id` and returns the
/// removed row, `None` when the pair names no note — a note of another
/// report is the same miss as no note at all.
///
/// Pleroma's `notes_delete` destroys by note id alone; keying the pair
/// here is what lets the 404 answer a note that is not this report's, and
/// what keeps one request from deleting across reports.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_note(
    pool: &PgPool,
    report_id: Uuid,
    note_id: Uuid,
) -> AppResult<Option<ReportNote>> {
    let row = sqlx::query_as::<_, ReportNote>(DELETE_NOTE_SQL)
        .bind(note_id)
        .bind(report_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use crate::{error::AppError, models::report::test_support::*};

    use super::*;

    /// Notes round-trip: [`create_note`] stamps the author and the text,
    /// [`notes_for`] reads them back oldest first per report, the batched
    /// read groups the same rows, and [`delete_note`] answers the removed
    /// row — only for its own report's pair.
    #[tokio::test]
    async fn notes_are_written_read_and_deleted_per_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let moddie = local_user(&pool, "moddie").await;
        let one = seed(&pool, &alice, &bob).await;
        let two = seed(&pool, &bob, &alice).await;

        let first = create_note(&pool, one.id, moddie.id, "looked")
            .await
            .expect("the first note writes");
        let second = create_note(&pool, one.id, moddie.id, "again")
            .await
            .expect("the second note writes");
        let _other = create_note(&pool, two.id, alice.id, "elsewhere")
            .await
            .expect("the foreign note writes");
        assert_eq!(first.id.get_version_num(), 7, "the note id is a UUIDv7");
        assert_eq!(first.report_id, one.id, "it names its report");
        assert_eq!(first.user_id, Some(moddie.id), "and its author");
        assert_eq!(first.content, "looked", "the text verbatim");

        let notes = notes_for(&pool, one.id).await.unwrap();
        assert_eq!(
            notes.iter().map(|note| note.id).collect::<Vec<Uuid>>(),
            vec![first.id, second.id],
            "oldest first"
        );
        let batched = notes_for_reports(&pool, &[one.id, two.id]).await.unwrap();
        assert_eq!(batched.len(), 3, "every note of both reports");
        assert!(
            notes_for_reports(&pool, &[]).await.unwrap().is_empty(),
            "no ids read nothing"
        );

        assert!(
            delete_note(&pool, two.id, first.id)
                .await
                .unwrap()
                .is_none(),
            "the foreign pair deletes nothing"
        );
        let removed = delete_note(&pool, one.id, first.id)
            .await
            .unwrap()
            .expect("the pair names the note");
        assert_eq!(removed, first, "the removed row comes back");
        assert_eq!(
            notes_for(&pool, one.id).await.unwrap().len(),
            1,
            "and it is gone"
        );
        assert!(
            delete_note(&pool, one.id, Uuid::now_v7())
                .await
                .unwrap()
                .is_none(),
            "an unknown note id is the same miss"
        );
    }

    /// A note's author and report are the model's guards: a report that
    /// names nothing is the foreign key's refusal, and deleting the
    /// report takes its notes with it.
    #[tokio::test]
    async fn a_note_names_a_real_report_and_cascades_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = seed(&pool, &alice, &bob).await;
        let _note = create_note(&pool, row.id, alice.id, "kept with the report")
            .await
            .expect("the note writes");

        let orphan = create_note(&pool, Uuid::now_v7(), alice.id, "nowhere")
            .await
            .expect_err("a report that names nothing is refused");
        assert!(
            matches!(orphan, AppError::Database(_)),
            "the foreign key is the guard: {orphan}"
        );

        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the target deletes");
        assert_eq!(gone.rows_affected(), 1, "the account goes");
        assert!(
            notes_for(&pool, row.id).await.unwrap().is_empty(),
            "and its report's notes cascaded away with it"
        );
    }
}
