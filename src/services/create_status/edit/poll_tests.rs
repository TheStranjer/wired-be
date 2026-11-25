//! The poll arm of [`edit_status`]: the `poll` parameter under the create's
//! rules, and what a restarted question leaves behind.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::{
    models::poll::{cast_vote, get_for_status, options},
    services::create_status::test_support::{created, posted, state, upload, user, with_media},
};

/// An edit body as `ApiBody` hands it over.
fn body(value: Value) -> EditStatusParams {
    crate::extract::into_params::<EditStatusParams>(value)
        .expect("every body in these tests fits the edit parameter type")
}

/// A poll of `a`, `b`, `c` posted by `author`, with the stored row.
async fn question(pool: &PgPool, app_state: &AppState, author: &User) -> (Status, status::Status) {
    let entity = created(
        app_state,
        author,
        CreateStatusParams {
            poll: Some(PollRequest {
                options: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
                expires_in: Some(3600),
                multiple: Some(true),
                hide_totals: None,
            }),
            ..posted("which?")
        },
    )
    .await;
    let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
    let row = status::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture post is stored");
    (entity, row)
}

/// How many notifications `recipient` holds.
async fn told(pool: &PgPool, recipient: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notifications WHERE user_id = $1")
        .bind(recipient)
        .fetch_one(pool)
        .await
        .expect("the notifications count")
}

#[test]
fn the_poll_parameter_reads_like_the_create_bodys() {
    assert!(body(json!({})).poll.is_none(), "no poll key, nothing said");
    let poll =
        body(json!({"poll": {"options": ["a", "b"], "multiple": "true", "expires_in": "600"}}))
            .poll
            .expect("the key is a poll request");
    assert_eq!(poll.options, vec!["a", "b"]);
    assert_eq!(poll.multiple, Some(true), "the form spelling reads");
    assert_eq!(poll.expires_in, Some(600));
}

#[tokio::test]
async fn new_options_restart_the_poll_tell_the_voter_and_snapshot_the_old_ones() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let app_state = state(pool);
    let alice = user(pool, "poll-service-author").await;
    let bob = user(pool, "poll-service-voter").await;
    let (entity, row) = question(pool, &app_state, &alice).await;
    let stored = get_for_status(pool, row.id)
        .await
        .unwrap()
        .expect("the poll is stored");
    let old = options(pool, stored.id).await.expect("the options read");
    let landed = cast_vote(pool, stored.id, bob.id, &[old[0].id])
        .await
        .expect("the vote casts");
    assert!(landed, "the fixture ballot counts");
    assert_eq!(told(pool, bob.id).await, 0, "a ballot tells nobody");

    let edited = edit_status(
        &app_state,
        &alice,
        &row,
        body(json!({"poll": {"options": ["n1", "n2"], "multiple": false}})),
    )
    .await
    .expect("the edit writes")
    .expect("the status is live");

    let value = serde_json::to_value(&edited).expect("the entity serializes");
    assert_eq!(
        value["poll"]["options"],
        json!([
            {"title": "n1", "votes_count": 0},
            {"title": "n2", "votes_count": 0},
        ]),
        "the new options, at zero"
    );
    assert_eq!(value["poll"]["multiple"], false);
    let before = serde_json::to_value(&entity).expect("the entity serializes");
    assert_eq!(
        value["poll"]["id"], before["poll"]["id"],
        "the same poll row, restarted"
    );
    assert_eq!(told(pool, bob.id).await, 1, "the voter hears the edit");
    assert_eq!(told(pool, alice.id).await, 0, "the author does not");
    let history = status_edit::list_for_status(pool, row.id)
        .await
        .expect("the revisions read");
    assert_eq!(history.len(), 1, "the old question is a version");
    assert_eq!(
        history[0].poll_options,
        Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
    );
}

#[tokio::test]
async fn an_edit_without_a_poll_keeps_the_poll_and_its_votes_and_tells_nobody() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let app_state = state(pool);
    let alice = user(pool, "poll-keep-author").await;
    let bob = user(pool, "poll-keep-voter").await;
    let (_entity, row) = question(pool, &app_state, &alice).await;
    let stored = get_for_status(pool, row.id)
        .await
        .unwrap()
        .expect("the poll is stored");
    let old = options(pool, stored.id).await.expect("the options read");
    let landed = cast_vote(pool, stored.id, bob.id, &[old[1].id])
        .await
        .expect("the vote casts");
    assert!(landed, "the fixture ballot counts");

    let edited = edit_status(
        &app_state,
        &alice,
        &row,
        body(json!({"status": "which one?"})),
    )
    .await
    .expect("the edit writes")
    .expect("the status is live");

    let value = serde_json::to_value(&edited).expect("the entity serializes");
    assert_eq!(value["poll"]["options"][1]["votes_count"], 1, "votes stay");
    assert_eq!(value["poll"]["votes_count"], 1);
    assert_eq!(told(pool, bob.id).await, 0, "nothing restarted");
}

#[tokio::test]
async fn a_refused_poll_edit_writes_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let app_state = state(pool);
    let alice = user(pool, "poll-refused-author").await;
    let (_entity, row) = question(pool, &app_state, &alice).await;

    for poll in [
        json!({"options": ["only"]}),
        json!({"options": ["a", ""]}),
        json!({"options": ["a", "b"], "expires_in": 1}),
    ] {
        let err = edit_status(&app_state, &alice, &row, body(json!({"poll": poll})))
            .await
            .expect_err("the create's poll rules apply to the edit");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a 422, as on create: {err}"
        );
    }
    let stored = get_for_status(pool, row.id)
        .await
        .unwrap()
        .expect("the poll is stored");
    let titles = options(pool, stored.id).await.expect("the options read");
    assert_eq!(titles.len(), 3, "the options are as they were");
    assert!(
        status_edit::list_for_status(pool, row.id)
            .await
            .expect("the revisions read")
            .is_empty(),
        "no revision was recorded"
    );
}

#[tokio::test]
async fn a_poll_cannot_be_added_to_a_post_with_media() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool();
    let app_state = state(pool);
    let alice = user(pool, "poll-media-author").await;
    let media = upload(pool, &alice).await;
    let id_text = media.id.to_string();
    let entity = created(
        &app_state,
        &alice,
        with_media("pictured", &[id_text.as_str()]),
    )
    .await;
    let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
    let row = status::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture post is stored");

    let err = edit_status(
        &app_state,
        &alice,
        &row,
        body(json!({"poll": {"options": ["a", "b"]}})),
    )
    .await
    .expect_err("questions are not asked over attachments");
    assert_eq!(err.to_string(), POLL_WITH_MEDIA);
    assert!(
        get_for_status(pool, row.id).await.unwrap().is_none(),
        "no poll was written"
    );
}
