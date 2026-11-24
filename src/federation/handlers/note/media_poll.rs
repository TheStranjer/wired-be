//! The attachment and poll halves of an edit: the stored media compared and
//! rewritten, and the remote poll totals written over the mirrored ones.

use sqlx::PgPool;
use uuid::Uuid;

use super::whole;
#[cfg(doc)]
use crate::models::status_edit;
use crate::{
    error::AppResult,
    federation::{
        dispatch::FederationError,
        note_parser::{ParsedAttachment, ParsedPoll},
    },
    models::poll::{self, RemoteTotals},
    services::ingest,
    state::AppState,
};

/// The post's attachments as stored, in render order: each row's id (what
/// the replacement deletes after the versioned write detaches it) beside
/// the url (what [`media_replacement_needed`] compares).
pub(super) async fn stored_media(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<(Uuid, String)>> {
    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, url FROM media_attachments
         WHERE status_id = $1 ORDER BY position, id",
    )
    .bind(status_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Whether the document's attachments differ from the post's stored ones:
/// the urls compared in order, so an unchanged restatement replaces nothing
/// — and, because [`status_edit::update_status`] only ever snapshots a
/// version it sees changing, records no revision for it either.
pub(super) fn media_replacement_needed(
    stored: &[(Uuid, String)],
    fresh: &[ParsedAttachment],
) -> bool {
    stored.len() != fresh.len()
        || stored
            .iter()
            .zip(fresh)
            .any(|((_id, url), media)| url != &media.url)
}

/// The attachment rewrite beside the versioned write: the detached rows of
/// a remote post are not uploads anybody may re-attach, so they go, and the
/// document's fresh set lands through the create path's own
/// [`ingest::insert_media`] — same row shape, same remote-url semantics,
/// whichever activity brought the media.
pub(super) async fn replace_media(
    state: &AppState,
    status_id: Uuid,
    old_ids: &[Uuid],
    attachments: &[ParsedAttachment],
) -> Result<(), FederationError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| FederationError::transient("the attachment rewrite failed", &err))?;
    sqlx::query("DELETE FROM media_attachments WHERE id = ANY($1)")
        .bind(old_ids)
        .execute(&mut *tx)
        .await
        .map(drop)
        .map_err(|err| FederationError::transient("the detached attachment delete failed", &err))?;
    ingest::insert_media(&mut tx, status_id, attachments).await?;
    tx.commit()
        .await
        .map_err(|err| FederationError::transient("the attachment rewrite commit failed", &err))?;
    Ok(())
}

/// The poll arm of [`fold_edit`]: a `Question` whose update restates its
/// poll has the remote's numbers written over the mirrored ones — options,
/// voter count, total and deadline — and a `Question` that arrived without
/// a poll grows one exactly the create path would have grown it. A document
/// carrying no poll moves nothing; the stored poll's own votes, cast here,
/// are nobody's to overwrite from there.
pub(super) async fn refresh_poll(
    state: &AppState,
    status_id: Uuid,
    question: Option<&ParsedPoll>,
) -> Result<(), FederationError> {
    let Some(poll_data) = question else {
        return Ok(());
    };
    let stored = poll::get_for_status(&state.db, status_id)
        .await
        .map_err(|err| FederationError::transient("the stored poll read failed", &err))?;
    if let Some(poll) = stored {
        let totals = remote_totals(poll_data);
        return poll::record_remote_totals(&state.db, poll.id, &totals)
            .await
            .map_err(|err| FederationError::transient("the remote poll totals failed", &err));
    }
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| FederationError::transient("the poll insert failed", &err))?;
    ingest::insert_poll(&mut tx, status_id, Some(poll_data)).await?;
    tx.commit()
        .await
        .map_err(|err| FederationError::transient("the poll insert commit failed", &err))?;
    Ok(())
}

/// The remote's report as [`RemoteTotals`]: each option its stated count,
/// the poll its `votersCount` and the sum of the options, the deadline as
/// the document names it (`None` keeps the stored one — see
/// [`poll::record_remote_totals`]).
fn remote_totals(poll_data: &ParsedPoll) -> RemoteTotals {
    let option_counts: Vec<(String, i32)> = poll_data
        .options
        .iter()
        .map(|option| (option.title.clone(), whole(option.votes_count)))
        .collect();
    let votes_count = option_counts.iter().fold(0_i64, |sum, (_title, votes)| {
        sum.saturating_add(i64::from(*votes))
    });
    RemoteTotals {
        options: option_counts,
        voters_count: poll_data.voters_count.map_or(0, whole),
        votes_count: whole(votes_count),
        expires_at: poll_data.expires_at,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::super::test_support::{
        app_state, author, author_again, end_time, json_parsed_poll, mirrored, question_update,
        view_of,
    };
    use super::super::update_note;
    use super::*;
    use crate::federation::dispatch::Outcome;

    #[tokio::test]
    async fn a_question_update_moves_the_poll_totals() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/poll-1";
        let row = mirrored(&pool, &author, uri, "tea or coffee?").await;
        let mut tx = pool.begin().await.expect("the poll fixture opens");
        ingest::insert_poll(
            &mut tx,
            row.id,
            Some(&json_parsed_poll(
                20,
                DateTime::parse_from_rfc3339("2024-06-01T00:00:00Z")
                    .expect("the deadline is an instant")
                    .with_timezone(&Utc),
            )),
        )
        .await
        .expect("the poll fixture lands");
        tx.commit().await.expect("the poll fixture commits");

        let outcome = update_note(
            state,
            view_of("Update", question_update(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the totals are folded");
        let stand = poll::get_for_status(&pool, row.id)
            .await
            .expect("the poll read")
            .expect("the post still carries its poll");
        assert_eq!(stand.voters_count, 42, "the remote's voter count wins");
        assert_eq!(stand.votes_count, 45, "and its vote total");
        let options = poll::options(&pool, stand.id)
            .await
            .expect("the options read");
        assert_eq!(
            options.iter().map(|o| o.votes_count).collect::<Vec<_>>(),
            vec![30, 15],
            "option by option, matched by title"
        );
        assert_eq!(
            stand.expires_at,
            Some(end_time()),
            "and the deadline the document named overwrote the stored one"
        );
    }

    #[tokio::test]
    async fn a_question_update_grows_the_poll_it_first_carries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/poll-late";
        let row = mirrored(&pool, &author, uri, "a plain question").await;

        let grow = update_note(state, view_of("Update", question_update(uri), None), author)
            .await
            .expect("the fold completes");
        assert_eq!(grow, Outcome::Handled, "the poll-bearing update folds");

        let stand = poll::get_for_status(&pool, row.id)
            .await
            .expect("the poll read")
            .expect("the update's poll landed");
        assert_eq!(stand.voters_count, 42, "as a mirrored create's would");
        assert_eq!(poll::options(&pool, stand.id).await.unwrap().len(), 2);
    }
}
