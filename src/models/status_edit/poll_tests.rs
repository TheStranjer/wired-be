//! The poll arm of [`update_status`]: a restarted question is a new version
//! whose revision keeps the old options, and a deadline-only edit is none.

use chrono::{TimeDelta, Utc};
use sqlx::PgPool;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        poll::{self, PollEdit, PollParams},
        status::{self, NewStatus},
    },
    test_fixtures::local_user,
};

/// A local status of `author` carrying the poll `titles`, plus the poll row.
async fn polled(
    pool: &PgPool,
    author: &crate::models::user::User,
    titles: &[&str],
) -> (Status, poll::Poll) {
    let posted = status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, "which?"))
        .await
        .expect("the fixture post inserts");
    let mut conn = pool.acquire().await.expect("slot connection");
    let stand = poll::create(
        &mut conn,
        posted.id,
        &PollParams {
            options: titles.iter().map(|title| (*title).to_owned()).collect(),
            ..PollParams::default()
        },
    )
    .await
    .expect("the fixture poll creates");
    (posted, stand)
}

/// The edit that leaves the status as it is and states `poll`.
fn restating(posted: &Status, poll: Option<PollEdit>) -> EditParams {
    EditParams {
        content: posted.content.clone(),
        raw_text: posted.raw_text.clone(),
        content_type: posted.content_type.clone(),
        spoiler_text: posted.spoiler_text.clone(),
        sensitive: posted.sensitive,
        language: posted.language.clone(),
        mentioned_user_ids: Vec::new(),
        silent_user_ids: Vec::new(),
        hashtags: Vec::new(),
        media_ids: None,
        edited_at: None,
        media_attributes: Vec::new(),
        media_refresh: Vec::new(),
        poll,
    }
}

/// A single-choice [`PollEdit`] of `titles` with no new deadline.
fn question(titles: &[&str]) -> PollEdit {
    PollEdit {
        options: titles.iter().map(|title| (*title).to_owned()).collect(),
        multiple: false,
        hide_totals: false,
        expires_at: None,
    }
}

#[tokio::test]
async fn a_restarted_poll_records_the_old_options_in_a_revision() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "poll_reviser").await;
    let (posted, stand) = polled(&pool, &alice, &["a", "b", "c"]).await;

    let updated = update_status(
        &pool,
        posted.id,
        restating(&posted, Some(question(&["n1", "n2"]))),
    )
    .await
    .expect("the edit writes")
    .expect("the status is live");
    assert!(
        updated.edited_at.is_some(),
        "the new question stamps an edit"
    );

    let history = list_for_status(&pool, posted.id)
        .await
        .expect("the revisions read");
    assert_eq!(history.len(), 1, "the old question is one revision");
    assert_eq!(
        history[0].poll_options,
        Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]),
        "the revision keeps the options it asked"
    );
    let titles = poll::options(&pool, stand.id).await.expect("reads");
    assert_eq!(titles.len(), 2, "the poll now has the new options");
}

#[tokio::test]
async fn a_deadline_only_poll_edit_records_no_revision() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "poll_extender").await;
    let (posted, stand) = polled(&pool, &alice, &["a", "b"]).await;

    let later = PollEdit {
        expires_at: Some(Utc::now() + TimeDelta::hours(3)),
        ..question(&["a", "b"])
    };
    let updated = update_status(&pool, posted.id, restating(&posted, Some(later)))
        .await
        .expect("the edit writes")
        .expect("the status is live");
    assert!(updated.edited_at.is_none(), "no version was superseded");
    assert!(
        list_for_status(&pool, posted.id)
            .await
            .expect("the revisions read")
            .is_empty(),
        "so no revision exists"
    );
    let kept = poll::get_for_status(&pool, posted.id)
        .await
        .expect("reads")
        .expect("the poll stays");
    assert_eq!(kept.id, stand.id);
    assert!(kept.expires_at.is_some(), "the deadline moved");
}

#[tokio::test]
async fn adding_a_poll_records_a_revision_without_options() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "poll_adder").await;
    let posted = status::insert(&pool, TEST_BASE_URL, &NewStatus::local(&alice, "plain"))
        .await
        .expect("the fixture post inserts");

    let updated = update_status(
        &pool,
        posted.id,
        restating(&posted, Some(question(&["x", "y"]))),
    )
    .await
    .expect("the edit writes")
    .expect("the status is live");
    assert!(updated.edited_at.is_some(), "a new poll is a new version");
    let history = list_for_status(&pool, posted.id)
        .await
        .expect("the revisions read");
    assert_eq!(history.len(), 1, "the poll-less version is on record");
    assert_eq!(history[0].poll_options, None, "it asked no question");
    assert!(
        poll::get_for_status(&pool, posted.id)
            .await
            .expect("reads")
            .is_some(),
        "and the poll now stands"
    );
}
