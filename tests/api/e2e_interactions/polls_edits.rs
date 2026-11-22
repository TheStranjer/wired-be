//! Scenarios 7-8: a poll ballot and the edit whose history keeps the old
//! text, each read back through every surface.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    POLLS, SECOND_BALLOT, STATUSES, assert_every, created, entity_id, follows, get, pick, post,
    posted, representations, shown, user,
};
use crate::common;

// ──────────────────────  Scenario (7): poll vote  ──────────────────────

/// A ballot through `POST /api/v1/polls/{id}/votes` moves the tallies the
/// status entity renders on every representation, shows the voter their
/// own marks and nobody else's, and the second ballot of one voter is the
/// already-voted 422 that counted nothing.
#[tokio::test]
async fn a_poll_ballot_moves_the_tallies_and_locks_a_second_vote() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let created = created(
        &app,
        &alice_token,
        json!({"status": "tea or coffee?", "poll": {"options": ["tea", "coffee"]}}),
    )
    .await;
    let id = entity_id(&created);
    let poll_id = pick(&created, &["poll", "id"])
        .as_str()
        .expect("a poll create answers the poll entity")
        .to_owned();
    assert_eq!(
        created["poll"]["votes_count"],
        json!(0),
        "the stand opens at zero"
    );
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let ballot = posted(
        &app,
        &bob_token,
        &format!("{POLLS}/{poll_id}/votes"),
        json!({"choices": [1]}),
    )
    .await;
    assert_ballot(&ballot, &poll_id);

    let one = json!(1);
    let mine = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &mine,
        &["poll", "votes_count"],
        &one,
        "the tallies ride every read",
    );
    assert_every(
        &mine,
        &["poll", "voted"],
        &json!(true),
        "and the voter sees their ballot",
    );
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["poll", "votes_count"],
        &one,
        "the public tallies",
    );
    assert_every(
        &strangers,
        &["poll", "own_votes"],
        &json!([]),
        "which name nobody else's marks",
    );

    let second = common::TestRequest::post(&format!("{POLLS}/{poll_id}/votes"))
        .bearer(&bob_token)
        .json(&json!({"choices": [0]}))
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::UNPROCESSABLE_ENTITY, "a second ballot");
    common::assert_error(&second, SECOND_BALLOT, "the already-voted refusal");
    let stand = shown(&app, Some(&bob_token), &id).await;
    assert_eq!(
        pick(&stand, &["poll", "votes_count"]),
        one,
        "the refusal counted nothing"
    );
    assert_eq!(
        pick(&stand, &["poll", "own_votes"]),
        json!([1]),
        "and rewrote nothing"
    );
}

/// The ballot's answer: the poll's own entity, `voted` for its voter with
/// position `1` marked, and the moved tallies on the marked option alone.
fn assert_ballot(stand: &Value, poll_id: &str) {
    assert_eq!(
        stand["id"],
        json!(poll_id),
        "the answer is the poll's entity"
    );
    assert_eq!(stand["voted"], json!(true), "flagged as cast");
    assert_eq!(stand["own_votes"], json!([1]), "with the voter's mark");
    assert_eq!(stand["votes_count"], json!(1), "and the moved roll-up");
    let options = stand["options"]
        .as_array()
        .expect("the poll answers its options");
    assert_eq!(options.len(), 2, "one entry per option");
    assert_eq!(
        options[1]["votes_count"],
        json!(1),
        "only the marked option gained"
    );
    assert_eq!(
        options[0]["votes_count"],
        json!(0),
        "and the other stayed at zero"
    );
}

// ─────────────────────────  Scenario (8): edit  ─────────────────────────

/// A `PUT /api/v1/statuses/{id}` rewrite rides every representation with
/// its new content and `edited_at` stamp, and the two reads behind it
/// keep the story: `…/history` answers the superseded text beside the
/// current one, and `…/source` the live status's raw source.
#[tokio::test]
async fn an_edit_rewrites_the_post_and_the_history_keeps_the_old_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let id = entity_id(&post(&app, &alice_token, "original words").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let response = common::TestRequest::put(&format!("{STATUSES}/{id}"))
        .bearer(&alice_token)
        .json(&json!({"status": "corrected words"}))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the author edits");
    let edited = response.json_value();
    assert_eq!(
        edited["content"],
        json!("<p>corrected words</p>"),
        "the write answers the rewrite"
    );
    assert!(
        edited["edited_at"].is_string(),
        "stamped as edited: {edited}"
    );

    let rewritten = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &rewritten,
        &["content"],
        &json!("<p>corrected words</p>"),
        "the rewrite rides every read",
    );

    let history = get(&app, Some(&bob_token), &format!("{STATUSES}/{id}/history"))
        .await
        .json_value();
    let items = history.as_array().expect("the history is an array");
    assert_eq!(
        items.len(),
        2,
        "the superseded version plus the current one"
    );
    assert_eq!(
        items[0]["content"],
        json!("<p>original words</p>"),
        "history keeps the old text first"
    );
    assert_eq!(
        items[1]["content"],
        json!("<p>corrected words</p>"),
        "and answers the current one last"
    );

    let source = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}/source"))
        .await
        .json_value();
    assert_eq!(source["id"], json!(id), "the source names the post");
    assert_eq!(
        source["text"],
        json!("corrected words"),
        "as the author's newest raw text"
    );
    assert_eq!(
        source["content_type"],
        json!("text/plain"),
        "in its stored type"
    );
}
