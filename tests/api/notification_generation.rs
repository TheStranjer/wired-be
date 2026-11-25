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
//! WBE-113 adds the four remaining generation types to the same view:
//!
//! - `poll` — a ballot tells nobody, and the `polls.end_notify` sweep tells the author
//!   (Pleroma's exemption: their own poll's closing is their news) and
//!   every local voter exactly once across reruns.
//! - `update` — an edit tells the post's local boosters, and nobody else.
//! - `status` — a post tells the author's *subscribers* (an accepted
//!   follow with its notify toggle on), never a `direct` one.
//! - `pleroma:emoji_reaction` — a reaction tells the status's author with
//!   its emoji, and the withdrawal takes that one telling back.
//!
//! The replied-to author's mention and the `expires`/scheduled parity are
//! the service's own unit-test territory (the create endpoint takes no
//! `in_reply_to_id` yet), see `src/services/notify.rs`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
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

/// A JSON-body `POST /api/v1/statuses`, answered 200 as its entity JSON.
async fn post_json(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post("/api/v1/statuses")
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the JSON status post");
    response.json_value()
}

/// The poll fixture: a public two-option question closing in five minutes,
/// answering `(status id, poll id)` as the entity spells them.
async fn create_poll(app: &Router, token: &str, text: &str) -> (String, String) {
    let created = post_json(
        app,
        token,
        json!({
            "status": text,
            "poll": {"options": ["tea", "coffee"], "expires_in": 300},
        }),
    )
    .await;
    (
        created["id"].as_str().expect("the posted id").to_owned(),
        created["poll"]["id"]
            .as_str()
            .expect("the poll entity")
            .to_owned(),
    )
}

/// `POST /api/v1/polls/{id}/votes` marking option 0, answered 200.
async fn vote(app: &Router, token: &str, poll_id: &str) {
    let response = vote_raw(app, token, poll_id).await;
    common::assert_answer(&response, StatusCode::OK, "the ballot");
}

/// The same write, unanswered: for the caller that asserts a refusal.
async fn vote_raw(app: &Router, token: &str, poll_id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/polls/{poll_id}/votes"))
        .bearer(token)
        .json(&json!({"choices": [0]}))
        .send(app)
        .await
}

/// Moves the poll's deadline to `mins` minutes in the past through the
/// table: the sweep's window reads the database clock, and the create's
/// minimum is five minutes, so SQL is the honest time machine.
async fn expire_poll(pool: &PgPool, poll_id: &str, mins: i32) {
    let id = Uuid::parse_str(poll_id).expect("the fixture poll id is an id");
    let moved = sqlx::query(
        "UPDATE polls SET expires_at = now() - make_interval(mins => $2) WHERE id = $1",
    )
    .bind(id)
    .bind(mins)
    .execute(pool)
    .await
    .expect("the deadline moves");
    assert_eq!(moved.rows_affected(), 1, "the fixture poll was there");
}

/// Runs one `polls.end_notify` sweep pass and requires it to succeed.
async fn run_end_notify(pool: &PgPool) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    let attempt = wired_be::jobs::AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };
    let outcome = wired_be::jobs::polls::handle_end_notify(state, attempt, json!({})).await;
    assert!(
        outcome.is_ok(),
        "the sweep pass answers Ok, not {outcome:?}"
    );
}

/// `PUT` (or `DELETE`, when `delete`) the caller's reaction with the
/// percent-encoded `{emoji}` segment on the status.
async fn react_write(app: &Router, token: &str, status_id: &str, emoji: &str, delete: bool) {
    let path = format!("/api/v1/pleroma/statuses/{status_id}/reactions/{emoji}");
    let request = if delete {
        common::TestRequest::delete(&path)
    } else {
        common::TestRequest::put(&path)
    };
    let response = request.bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "the reaction write");
}

/// The emojis of the caller's `pleroma:emoji_reaction` listings, in order.
async fn reaction_emojis(app: &Router, token: &str) -> Vec<String> {
    listed(app, token)
        .await
        .iter()
        .filter(|entry| entry["type"] == "pleroma:emoji_reaction")
        .map(|entry| {
            entry["emoji"]
                .as_str()
                .expect("a reaction carries its emoji")
                .to_owned()
        })
        .collect()
}

/// The two unicode emoji the reaction tests send, and the percent-encoded
/// path segments a client puts in their place.
const THUMB: &str = "👍";
const THUMB_SEGMENT: &str = "%F0%9F%91%8D";
const PARTY: &str = "🎉";
const PARTY_SEGMENT: &str = "%F0%9F%8E%89";

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

#[tokio::test]
async fn a_poll_vote_tells_the_author_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = fixture(&pool, "gnv-alice").await;
    let (_bob, bob_token) = fixture(&pool, "gnv-bob").await;
    let (_carol, carol_token) = fixture(&pool, "gnv-carol").await;

    let (_status_id, poll_id) = create_poll(&app, &alice_token, "tea or coffee?").await;
    vote(&app, &bob_token, &poll_id).await;
    // The endpoint's own refusal: a second ballot from bob is 422.
    let again = vote_raw(&app, &bob_token, &poll_id).await;
    common::assert_answer(
        &again,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the repeat ballot",
    );
    vote(&app, &carol_token, &poll_id).await;

    assert!(
        listed(&app, &alice_token).await.is_empty(),
        "no ballot raises a poll notification for the author"
    );
    assert!(
        listed(&app, &bob_token).await.is_empty() && listed(&app, &carol_token).await.is_empty(),
        "the voters are told nothing by each other's ballots"
    );
}

#[tokio::test]
async fn the_poll_end_sweep_notifies_author_and_voters_exactly_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "gne-alice").await;
    let (_bob, bob_token) = fixture(&pool, "gne-bob").await;

    let (status_id, poll_id) = create_poll(&app, &alice_token, "closing soon").await;
    vote(&app, &bob_token, &poll_id).await;
    assert!(
        listed(&app, &alice_token).await.is_empty(),
        "the ballot told the author nothing while the poll is open"
    );
    expire_poll(&pool, &poll_id, 1).await;

    run_end_notify(&pool).await;
    let closing = (
        "poll".to_owned(),
        alice.id.to_string(),
        Some(status_id.clone()),
    );
    assert_eq!(
        facts(&app, &alice_token).await,
        vec![closing.clone()],
        "the author gains the one closing (actor: themselves)"
    );
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![closing.clone()],
        "the local voter is told from the author"
    );

    // The next minute's pass — and every pass after it — changes nothing:
    // the poll's flag retired it.
    run_end_notify(&pool).await;
    assert_eq!(
        facts(&app, &alice_token).await,
        vec![closing.clone()],
        "the rerun tells nobody twice"
    );
    assert_eq!(
        facts(&app, &bob_token).await,
        vec![closing],
        "and neither the author nor the voter grows a row"
    );
}

#[tokio::test]
async fn a_status_edit_notifies_rebloggers_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "ged-alice").await;
    let (_bob, bob_token) = fixture(&pool, "ged-bob").await;
    let (_carol, carol_token) = fixture(&pool, "ged-carol").await;

    let post = common::create_status(&pool, &alice, "worth boosting", Visibility::Public).await;
    let status_id = post.id.to_string();
    status_action(&app, &bob_token, "reblog", &status_id).await;
    follow(&app, &carol_token, &alice).await;
    // A plain follower is not a booster, and a booster's later withdrawal
    // would take them off the audience the same way.
    let edited = common::TestRequest::put(&format!("/api/v1/statuses/{status_id}"))
        .bearer(&alice_token)
        .form(&[("status", "the edited text")])
        .send(&app)
        .await;
    common::assert_answer(&edited, StatusCode::OK, "the edit write");

    assert_eq!(
        facts(&app, &bob_token).await,
        vec![("update".to_owned(), alice.id.to_string(), Some(status_id))],
        "the booster is told, naming the edited original"
    );
    assert!(
        listed(&app, &carol_token).await.is_empty(),
        "the plain follower is told nothing by the edit"
    );
    let alice_facts: Vec<String> = facts(&app, &alice_token)
        .await
        .into_iter()
        .map(|(kind, _actor, _status)| kind)
        .collect();
    assert_eq!(
        alice_facts,
        vec!["follow".to_owned(), "reblog".to_owned()],
        "the author keeps the boost's and the follow's tellings, and her \
         own edit adds no third: nobody is told of their own action"
    );
}

#[tokio::test]
async fn subscribers_are_told_by_visibility_rules() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "gsu-alice").await;
    let (_bob, bob_token) = fixture(&pool, "gsu-bob").await;
    let (_carol, carol_token) = fixture(&pool, "gsu-carol").await;

    // Bob subscribes; Carol follows plainly.
    let subscribed = common::TestRequest::post(&format!("/api/v1/accounts/{}/follow", alice.id))
        .bearer(&bob_token)
        .form(&[("notify", "true")])
        .send(&app)
        .await;
    common::assert_answer(&subscribed, StatusCode::OK, "the subscribing follow");
    follow(&app, &carol_token, &alice).await;

    let mut posted: Vec<String> = Vec::new();
    for visibility in ["public", "unlisted", "private", "local"] {
        let created = post_json(
            &app,
            &alice_token,
            json!({"status": format!("{visibility} news"), "visibility": visibility}),
        )
        .await;
        posted.push(created["id"].as_str().expect("the posted id").to_owned());
    }
    let want: Vec<_> = posted
        .iter()
        .rev()
        .map(|id| ("status".to_owned(), alice.id.to_string(), Some(id.clone())))
        .collect();
    assert_eq!(
        facts(&app, &bob_token).await,
        want,
        "the subscriber is told of every visibility but direct, newest first"
    );
    assert_eq!(
        facts(&app, &carol_token)
            .await
            .into_iter()
            .filter(|(kind, _actor, _status)| kind == "status")
            .count(),
        0,
        "the plain follower is told nothing"
    );

    // A direct post is never a subscriber's news.
    let _dm = post_json(
        &app,
        &alice_token,
        json!({"status": "addressees only", "visibility": "direct"}),
    )
    .await;
    assert_eq!(
        facts(&app, &bob_token)
            .await
            .into_iter()
            .filter(|(kind, _actor, _status)| kind == "status")
            .count(),
        4,
        "the direct post grew no fifth subscription telling"
    );
}

#[tokio::test]
async fn an_emoji_reaction_notifies_the_author_and_the_removal_retires_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = fixture(&pool, "gxr-alice").await;
    let (bob, bob_token) = fixture(&pool, "gxr-bob").await;

    let post = common::create_status(&pool, &alice, "react to me", Visibility::Public).await;
    let status_id = post.id.to_string();

    react_write(&app, &bob_token, &status_id, THUMB_SEGMENT, false).await;
    assert_eq!(
        facts(&app, &alice_token).await,
        vec![(
            "pleroma:emoji_reaction".to_owned(),
            bob.id.to_string(),
            Some(status_id.clone())
        )],
        "the reaction is told to the author, from the reactor, about the status"
    );
    assert_eq!(
        reaction_emojis(&app, &alice_token).await,
        vec![THUMB.to_owned()],
        "the entity carries the emoji"
    );

    // The idempotent repeat restates the fact, not a second telling.
    react_write(&app, &bob_token, &status_id, THUMB_SEGMENT, false).await;
    assert_eq!(
        reaction_emojis(&app, &alice_token).await,
        vec![THUMB.to_owned()],
        "a restated reaction tells once"
    );

    // A second emoji is a second telling; withdrawing the first takes only
    // the first back.
    react_write(&app, &bob_token, &status_id, PARTY_SEGMENT, false).await;
    let after_two = reaction_emojis(&app, &alice_token).await;
    assert_eq!(
        after_two.len(),
        2,
        "two emojis are two tellings: {after_two:?}"
    );
    assert!(
        after_two.contains(&THUMB.to_owned()) && after_two.contains(&PARTY.to_owned()),
        "both emojis stand: {after_two:?}"
    );

    react_write(&app, &bob_token, &status_id, THUMB_SEGMENT, true).await;
    assert_eq!(
        reaction_emojis(&app, &alice_token).await,
        vec![PARTY.to_owned()],
        "the withdrawal retires exactly its emoji's telling"
    );
    react_write(&app, &bob_token, &status_id, PARTY_SEGMENT, true).await;
    assert!(
        listed(&app, &alice_token).await.is_empty(),
        "and the second withdrawal empties the inbox"
    );
}

#[tokio::test]
async fn a_stranger_is_withheld_from_a_gated_account_under_the_new_types_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = fixture(&pool, "gna-alice").await;
    let (bob, bob_token) = fixture(&pool, "gna-bob").await;

    let setting = common::TestRequest::put("/api/pleroma/notification_settings")
        .bearer(&bob_token)
        .json(&json!({"block_from_strangers": true}))
        .send(&app)
        .await;
    common::assert_answer(&setting, StatusCode::OK, "the setting write");

    // Bob owns a post alice reacts to: the telling names a stranger and
    // stands withheld, exactly like the favourite, mention and follow of
    // the setting's original test.
    let own = common::create_status(&pool, &bob, "gate my post", Visibility::Public).await;
    react_write(
        &app,
        &alice_token,
        &own.id.to_string(),
        THUMB_SEGMENT,
        false,
    )
    .await;

    assert!(
        listed(&app, &bob_token).await.is_empty(),
        "a stranger's reaction is withheld by the setting"
    );
}
