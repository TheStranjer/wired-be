//! Notification generation over HTTP (WBE-110): the actions that tell an
//! account something happened, observed through
//! `GET /api/v1/notifications`.
//!
//! Every test drives the *causing* endpoint the way a client does — the
//! follow write, the `authorize` answer, `POST /api/v1/statuses` with
//! @-mentions, the favourite and reblog writes and their undos — and then
//! reads the recipient's flat listing. The tellings themselves are the
//! [`wired_be::services::notify`] module's, landed from the action hooks
//! of `src/api/accounts.rs`, `src/api/follow_requests.rs`,
//! `src/api/statuses.rs` and `src/services/create_status.rs`, and what
//! this file pins is that an action through
//! HTTP produces exactly one notification of the right type, naming the
//! right account and status:
//!
//! - `follow` / `follow_request` name the actor and no status; a locked
//!   account first sees the request, and after `authorize` the request row
//!   is gone and a `follow` row stands in its place.
//! - `mention` names the posting status, and every local user the text
//!   @-mentions receives one.
//! - `favourite` and `reblog` name the original status; the undo of
//!   either takes the telling back, and a repeat of either (which the
//!   writes already answer idempotently) never grows a second row.
//! - Self-actions — liking, boosting or mentioning oneself — notify
//!   nobody.
//! - A muted actor's tellings hide from the listing (and read back under
//!   `with_muted`), and a blocked actor's hide from it forever: the read
//!   scope of [`wired_be::models::notification`] is what suppresses them.
//!
//! The replied-to author's mention and the `expires`/scheduled parity are
//! the service's own unit-test territory (the create endpoint takes no
//! `in_reply_to_id` yet), see `src/services/notify.rs`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use wired_be::models::{
    status::Visibility,
    user::{Flag, NewUser, User},
};

use crate::common;

/// The flat listing every assertion reads.
const LIST: &str = "/api/v1/notifications";
/// The scopes the actors need: the writes, the follow routes and the
/// recipient's read of their own inbox.
const SCOPES: &str = "read write follow";

/// A local user named `nickname` beside a bearer token granting `SCOPES`.
async fn fixture(pool: &PgPool, nickname: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, SCOPES).await;
    (user, token)
}

/// A local user named `nickname` who answers every follow with a request,
/// beside its token.
async fn locked_fixture(pool: &PgPool, nickname: &str) -> (User, String) {
    let user = common::create_local_user_with(pool, nickname, |new| NewUser {
        locked: Flag::new(true),
        ..new
    })
    .await;
    let token = common::create_app_and_token(pool, &user, SCOPES).await;
    (user, token)
}

/// The caller's notifications as JSON entities, in listing order.
async fn listed(app: &Router, token: &str) -> Vec<Value> {
    listed_query(app, token, "").await
}

/// The listing under `query` (`?with_muted=true` and the like).
async fn listed_query(app: &Router, token: &str, query: &str) -> Vec<Value> {
    let response = common::TestRequest::get(&format!("{LIST}{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the notification listing");
    response
        .json_value()
        .as_array()
        .cloned()
        .expect("the listing answers an array")
}

/// The listing narrowed to `(type, actor id, status id)` triples: what
/// every assertion below compares.
async fn facts(app: &Router, token: &str) -> Vec<(String, String, Option<String>)> {
    listed(app, token)
        .await
        .iter()
        .map(|entry| {
            (
                entry["type"].as_str().expect("a type").to_owned(),
                entry["account"]["id"]
                    .as_str()
                    .expect("an actor id")
                    .to_owned(),
                entry["status"]["id"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// `POST /api/v1/accounts/{id}/follow`.
async fn follow(app: &Router, token: &str, target: &User) {
    let response = common::TestRequest::post(&format!("/api/v1/accounts/{}/follow", target.id))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the follow write");
}

/// `POST /api/v1/accounts/{id}/unfollow`.
async fn unfollow(app: &Router, token: &str, target: &User) {
    let response = common::TestRequest::post(&format!("/api/v1/accounts/{}/unfollow", target.id))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the unfollow write");
}

/// `POST /api/v1/accounts/{id}/mute` under `token`'s account.
async fn mute(app: &Router, token: &str, target: &User) {
    let response = common::TestRequest::post(&format!("/api/v1/accounts/{}/mute", target.id))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the mute write");
}

/// `POST /api/v1/accounts/{id}/block` under `token`'s account.
async fn block(app: &Router, token: &str, target: &User) {
    let response = common::TestRequest::post(&format!("/api/v1/accounts/{}/block", target.id))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the block write");
}

/// The write `verb` (`favourite`, `unfavourite`, `reblog`, `unreblog`) on
/// status `status_id`, answered 200.
async fn status_action(app: &Router, token: &str, verb: &str, status_id: &str) {
    let response = common::TestRequest::post(&format!("/api/v1/statuses/{status_id}/{verb}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, verb);
}

/// `POST /api/v1/follow_requests/{id}/authorize` (or `reject`), answered 200.
async fn answer_request(app: &Router, token: &str, requester: &User, verb: &str) {
    let response = common::TestRequest::post(&format!(
        "/api/v1/follow_requests/{}/{}",
        requester.id, verb
    ))
    .bearer(token)
    .send(app)
    .await;
    common::assert_answer(&response, StatusCode::OK, "the follow-request answer");
}

/// A status `token`'s account posts through `POST /api/v1/statuses`,
/// answered as its entity JSON.
async fn post_through_api(app: &Router, token: &str, text: &str) -> Value {
    let response = common::TestRequest::post("/api/v1/statuses")
        .bearer(token)
        .form(&[("status", text)])
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status post");
    response.json_value()
}

#[tokio::test]
async fn a_follow_notifies_the_followed_account_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "gn-alice").await;
    let (bob, bob_token) = fixture(&pool, "gn-bob").await;

    follow(&app, &alice_token, &bob).await;
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![("follow".to_owned(), alice.id.to_string(), None)],
        "the follow names alice and no status"
    );

    follow(&app, &alice_token, &bob).await;
    assert_eq!(
        listed(&app, &bob_token).await.len(),
        1,
        "a follow of an open pair tells once"
    );

    unfollow(&app, &alice_token, &bob).await;
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![("follow".to_owned(), alice.id.to_string(), None)],
        "the withdrawal takes no telling back"
    );
}

#[tokio::test]
async fn a_locked_account_sees_a_request_then_the_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "gr-alice").await;
    let (bob, bob_token) = locked_fixture(&pool, "gr-bob").await;

    follow(&app, &alice_token, &bob).await;
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![("follow_request".to_owned(), alice.id.to_string(), None)],
        "a locked account is told a request, not a follow"
    );

    follow(&app, &alice_token, &bob).await;
    assert_eq!(
        listed(&app, &bob_token).await.len(),
        1,
        "a repeated request tells once too"
    );

    answer_request(&app, &bob_token, &alice, "authorize").await;
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![("follow".to_owned(), alice.id.to_string(), None)],
        "the request telling became a follow telling"
    );
}

#[tokio::test]
async fn a_rejected_request_takes_nothing_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "rj-alice").await;
    let (bob, bob_token) = locked_fixture(&pool, "rj-bob").await;

    follow(&app, &alice_token, &bob).await;
    answer_request(&app, &bob_token, &alice, "reject").await;
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![("follow_request".to_owned(), alice.id.to_string(), None)],
        "a rejection takes no telling back: the request row stands"
    );
}

#[tokio::test]
async fn a_mention_notifies_every_local_mentioned_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "mt-alice").await;
    let (_bob, bob_token) = fixture(&pool, "mt-bob").await;
    let (_carol, carol_token) = fixture(&pool, "mt-carol").await;

    let posted = post_through_api(&app, &alice_token, "hello @mt-bob and @mt-carol").await;
    let status_id = posted["id"].as_str().expect("the posted id").to_owned();

    for (who, token) in [("bob", &bob_token), ("carol", &carol_token)] {
        assert_eq!(
            facts(&app, token).await,
            vec![(
                "mention".to_owned(),
                alice.id.to_string(),
                Some(status_id.clone())
            )],
            "{who} was @-mentioned and is told about the posting status"
        );
    }
    assert!(
        listed(&app, &alice_token).await.is_empty(),
        "the author is told nothing about their own post"
    );
}

#[tokio::test]
async fn a_favourite_notifies_the_author_and_the_undo_retires_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "fa-alice").await;
    let (bob, bob_token) = fixture(&pool, "fa-bob").await;
    let post = common::create_status(&pool, &alice, "like me", Visibility::Public).await;

    status_action(&app, &bob_token, "favourite", &post.id.to_string()).await;
    assert_eq!(
        facts(&app, &alice_token).await,
        vec![(
            "favourite".to_owned(),
            bob.id.to_string(),
            Some(post.id.to_string())
        )],
        "the like is told to the author, about the liked status"
    );

    status_action(&app, &bob_token, "favourite", &post.id.to_string()).await;
    assert_eq!(
        listed(&app, &alice_token).await.len(),
        1,
        "the repeat favourite grows no second row"
    );

    status_action(&app, &bob_token, "unfavourite", &post.id.to_string()).await;
    assert!(
        listed(&app, &alice_token).await.is_empty(),
        "the undo takes the telling back"
    );
}

#[tokio::test]
async fn a_reblog_notifies_the_original_author_and_the_undo_retires_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "rb-alice").await;
    let (bob, bob_token) = fixture(&pool, "rb-bob").await;
    let post = common::create_status(&pool, &alice, "boost me", Visibility::Public).await;

    status_action(&app, &bob_token, "reblog", &post.id.to_string()).await;
    assert_eq!(
        facts(&app, &alice_token).await,
        vec![(
            "reblog".to_owned(),
            bob.id.to_string(),
            Some(post.id.to_string())
        )],
        "the boost is told about the ORIGINAL, not the boost row"
    );

    status_action(&app, &bob_token, "reblog", &post.id.to_string()).await;
    assert_eq!(
        listed(&app, &alice_token).await.len(),
        1,
        "the repeat boost grows no second row"
    );

    status_action(&app, &bob_token, "unreblog", &post.id.to_string()).await;
    assert!(
        listed(&app, &alice_token).await.is_empty(),
        "the undo takes the telling back"
    );
}

#[tokio::test]
async fn self_actions_notify_nobody() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = fixture(&pool, "sa-alice").await;

    let post = post_through_api(&app, &alice_token, "talking to @sa-alice").await;
    let status_id = post["id"].as_str().expect("the posted id").to_owned();
    status_action(&app, &alice_token, "favourite", &status_id).await;
    status_action(&app, &alice_token, "reblog", &status_id).await;

    assert!(
        listed(&app, &alice_token).await.is_empty(),
        "liking, boosting or mentioning yourself tells yourself nothing"
    );
}

#[tokio::test]
async fn a_muted_actor_is_hidden_by_default_and_reads_back_under_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "mu-alice").await;
    let (bob, bob_token) = fixture(&pool, "mu-bob").await;
    let post = common::create_status(&pool, &bob, "mute me not", Visibility::Public).await;

    status_action(&app, &alice_token, "favourite", &post.id.to_string()).await;
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![(
            "favourite".to_owned(),
            alice.id.to_string(),
            Some(post.id.to_string())
        )],
        "the like is told before the mute"
    );

    mute(&app, &bob_token, &alice).await;
    assert!(
        listed(&app, &bob_token).await.is_empty(),
        "a notification-mute hides the actor's tellings"
    );
    let back = listed_query(&app, &bob_token, "?with_muted=true").await;
    assert_eq!(
        back.iter()
            .map(|entry| entry["type"].as_str().expect("a type").to_owned())
            .collect::<Vec<_>>(),
        vec!["favourite".to_owned()],
        "and `with_muted` uncovers what the mute only hid"
    );
}

#[tokio::test]
async fn a_stranger_is_withheld_from_an_account_that_blocks_them() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = fixture(&pool, "bs-alice").await;
    let (bob, bob_token) = fixture(&pool, "bs-bob").await;

    let setting = common::TestRequest::put("/api/pleroma/notification_settings")
        .bearer(&bob_token)
        .json(&serde_json::json!({"block_from_strangers": true}))
        .send(&app)
        .await;
    common::assert_answer(&setting, StatusCode::OK, "the setting write");

    let post = common::create_status(&pool, &bob, "gate me", Visibility::Public).await;
    status_action(&app, &alice_token, "favourite", &post.id.to_string()).await;
    let _mentioned = post_through_api(&app, &alice_token, "hello @bs-bob").await;
    follow(&app, &alice_token, &bob).await;
    assert!(
        listed(&app, &bob_token).await.is_empty(),
        "a stranger's like, mention and follow are all withheld by the setting"
    );
}

#[tokio::test]
async fn a_blocked_actor_is_hidden_even_under_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "bl-alice").await;
    let (bob, bob_token) = fixture(&pool, "bl-bob").await;
    let post = common::create_status(&pool, &bob, "block me not", Visibility::Public).await;

    status_action(&app, &alice_token, "favourite", &post.id.to_string()).await;
    assert_eq!(
        listed(&app, &bob_token).await.len(),
        1,
        "the like is told before the block"
    );

    block(&app, &bob_token, &alice).await;
    assert!(
        listed(&app, &bob_token).await.is_empty(),
        "the block hides the actor's tellings"
    );
    assert!(
        listed_query(&app, &bob_token, "?with_muted=true")
            .await
            .is_empty(),
        "and `with_muted` never uncovers a block"
    );
}
