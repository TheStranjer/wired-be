//! `RESTRICT_UNAUTHENTICATED` with `activities_local` / `activities_remote`:
//! the anonymous status reads of the restricted locality answer the shared
//! 404 (or leave the status out of a listing), a presented user token reads
//! as before, and the other locality stays open.

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
};

use super::support::{
    NOT_FOUND, SCOPES, batch, context, context_ids, entity_ids, id_query, ids_of, quote_of,
    reblog_of, show, user_with_token,
};
use crate::common;

/// The id-keyed reads that share the single-status gate.
const GATED_READS: [&str; 7] = [
    "",
    "/context",
    "/card",
    "/history",
    "/favourited_by",
    "/reblogged_by",
    "/quotes",
];

/// A public post by a fresh remote account, as federation stores it.
async fn remote_post(pool: &PgPool, nickname: &str) -> (User, Status) {
    let author = common::create_remote_user(pool, nickname, "remote.example").await;
    let row = status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(format!("https://remote.example/objects/{nickname}")),
            user_id: author.id,
            content: "<p>from elsewhere</p>".to_owned(),
            raw_text: "from elsewhere".to_owned(),
            visibility: Visibility::Public,
            local: false,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the fixture remote post inserts");
    (author, row)
}

/// Asserts `response` is the shared 404 a closed or hidden status answers.
fn assert_closed(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::NOT_FOUND, who);
    common::assert_error(response, NOT_FOUND, who);
}

/// `GET /api/v1/statuses/{id}{suffix}` as `token` (`None`: anonymous).
async fn read(app: &Router, token: Option<&str>, id: &str, suffix: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}{suffix}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `activities_local` closes every anonymous read of a local status; a
/// user token still reads it, and a remote status stays open.
#[tokio::test]
async fn activities_local_closes_anonymous_reads_of_a_local_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app_with_restricted_areas(pool.clone(), &["activities_local"]);
    let local = common::create_status(&pool, &alice, "ours", Visibility::Public).await;
    let (_author, remote) = remote_post(&pool, "carol").await;
    let local_id = local.id.to_string();

    for suffix in GATED_READS {
        let anonymous = read(&app, None, &local_id, suffix).await;
        assert_closed(
            &anonymous,
            &format!("anonymous `{suffix}` of a local status"),
        );
        let signed_in = read(&app, Some(bob_token.as_str()), &local_id, suffix).await;
        common::assert_answer(
            &signed_in,
            StatusCode::OK,
            &format!("a user token's `{suffix}` of a local status"),
        );
    }

    let open = show(&app, None, &remote.id.to_string()).await;
    common::assert_answer(&open, StatusCode::OK, "anonymous show of a remote status");
}

/// `activities_remote` closes the remote half and leaves the local one.
#[tokio::test]
async fn activities_remote_closes_anonymous_reads_of_a_remote_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app_with_restricted_areas(pool.clone(), &["activities_remote"]);
    let local = common::create_status(&pool, &alice, "ours", Visibility::Public).await;
    let (_author, remote) = remote_post(&pool, "carol").await;
    let remote_id = remote.id.to_string();

    let anonymous = show(&app, None, &remote_id).await;
    assert_closed(&anonymous, "anonymous show of a remote status");
    let signed_in = show(&app, Some(bob_token.as_str()), &remote_id).await;
    common::assert_answer(&signed_in, StatusCode::OK, "a user token's remote show");
    let open = show(&app, None, &local.id.to_string()).await;
    common::assert_answer(&open, StatusCode::OK, "anonymous show of a local status");
}

/// With no `activities_*` area listed, anonymous reads of both stay open.
#[tokio::test]
async fn an_unrestricted_instance_keeps_both_halves_open() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = user_with_token(&pool, "alice", SCOPES).await;
    let app = common::test_app_with_restricted_areas(pool.clone(), &["timelines_local"]);
    let local = common::create_status(&pool, &alice, "ours", Visibility::Public).await;
    let (_author, remote) = remote_post(&pool, "carol").await;

    for row in [&local, &remote] {
        let response = show(&app, None, &row.id.to_string()).await;
        common::assert_answer(
            &response,
            StatusCode::OK,
            "an unrelated area restricts nothing",
        );
    }
}

/// A reblog is closed by its own locality and by its original's.
#[tokio::test]
async fn a_reblog_is_closed_by_its_original_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = user_with_token(&pool, "alice", SCOPES).await;
    let (_author, remote) = remote_post(&pool, "carol").await;
    let local = common::create_status(&pool, &alice, "ours", Visibility::Public).await;
    // A local reblog of a remote post, and (below) a remote account's of a
    // local one are each half local, half remote.
    let local_reblog_of_remote = reblog_of(&pool, &alice, &remote).await;
    let remote_booster = common::create_remote_user(&pool, "dave", "remote.example").await;
    let remote_reblog_of_local = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some("https://remote.example/objects/boost".to_owned()),
            user_id: remote_booster.id,
            reblog_of_id: Some(local.id),
            local: false,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the fixture remote reblog inserts");

    let remote_closed =
        common::test_app_with_restricted_areas(pool.clone(), &["activities_remote"]);
    let by_original = show(&remote_closed, None, &local_reblog_of_remote.id.to_string()).await;
    assert_closed(
        &by_original,
        "a local reblog of a remote post, remote closed",
    );
    let by_itself = show(&remote_closed, None, &remote_reblog_of_local.id.to_string()).await;
    assert_closed(&by_itself, "a remote reblog of a local post, remote closed");

    let local_closed = common::test_app_with_restricted_areas(pool.clone(), &["activities_local"]);
    let by_own_locality = show(&local_closed, None, &local_reblog_of_remote.id.to_string()).await;
    assert_closed(
        &by_own_locality,
        "a local reblog of a remote post, local closed",
    );
    let by_local_original = show(&local_closed, None, &remote_reblog_of_local.id.to_string()).await;
    assert_closed(
        &by_local_original,
        "a remote reblog of a local post, local closed",
    );
}

/// The batch read drops the closed locality for an anonymous caller and
/// keeps it for a user token.
#[tokio::test]
async fn the_batch_omits_the_closed_locality_anonymously() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app_with_restricted_areas(pool.clone(), &["activities_local"]);
    let local = common::create_status(&pool, &alice, "ours", Visibility::Public).await;
    let (_author, remote) = remote_post(&pool, "carol").await;
    let reblog = reblog_of(&pool, &alice, &remote).await;

    let only_local = id_query("id", &[local.id.to_string()]);
    let anonymous = batch(&app, None, &only_local).await;
    common::assert_answer(&anonymous, StatusCode::OK, "the anonymous local batch");
    assert_eq!(
        anonymous.json_value(),
        Value::Array(Vec::new()),
        "nothing left"
    );

    let mixed = id_query(
        "id",
        &[
            local.id.to_string(),
            remote.id.to_string(),
            reblog.id.to_string(),
        ],
    );
    let open_half = batch(&app, None, &mixed).await;
    assert_eq!(
        entity_ids(&open_half.json_value()),
        ids_of(&[&remote]),
        "the remote post stays; the local post and the local reblog go"
    );
    let signed_in = batch(&app, Some(bob_token.as_str()), &mixed).await;
    assert_eq!(
        entity_ids(&signed_in.json_value()),
        ids_of(&[&local, &remote, &reblog]),
        "a user token reads all three"
    );
}

/// Anonymous context arms omit the closed locality; the signed-in ones keep
/// it.
#[tokio::test]
async fn context_arms_omit_the_closed_locality_anonymously() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app_with_restricted_areas(pool.clone(), &["activities_local"]);
    let (author, root) = remote_post(&pool, "carol").await;
    let remote_reply = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some("https://remote.example/objects/reply".to_owned()),
            user_id: author.id,
            in_reply_to_id: Some(root.id),
            local: false,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the fixture remote reply inserts");
    let local_reply = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            in_reply_to_id: Some(root.id),
            in_reply_to_account_id: Some(author.id),
            ..NewStatus::local(&alice, "a local reply")
        },
    )
    .await
    .expect("the fixture local reply inserts");

    let anonymous = context(&app, None, &root.id.to_string()).await;
    common::assert_answer(&anonymous, StatusCode::OK, "the anonymous context");
    assert_eq!(
        context_ids(&anonymous.json_value(), "descendants"),
        ids_of(&[&remote_reply]),
        "the local reply is left out anonymously"
    );
    let signed_in = context(&app, Some(bob_token.as_str()), &root.id.to_string()).await;
    assert_eq!(
        context_ids(&signed_in.json_value(), "descendants"),
        ids_of(&[&remote_reply, &local_reply]),
        "a user token reads the whole thread"
    );

    let ancestors = context(&app, None, &remote_reply.id.to_string()).await;
    assert_eq!(
        context_ids(&ancestors.json_value(), "ancestors"),
        ids_of(&[&root]),
        "an open ancestor is kept"
    );
}

/// The quoters listing drops a closed locality's quoters anonymously.
#[tokio::test]
async fn the_quotes_listing_omits_the_closed_locality_anonymously() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let app = common::test_app_with_restricted_areas(pool.clone(), &["activities_local"]);
    let (_author, remote) = remote_post(&pool, "carol").await;
    let quote = quote_of(&pool, &alice, &remote, Visibility::Public).await;
    let path = format!("/api/v1/statuses/{}/quotes", remote.id);

    let anonymous = common::TestRequest::get(&path).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::OK, "the anonymous quotes listing");
    assert_eq!(
        anonymous.json_value(),
        Value::Array(Vec::new()),
        "no local quoter"
    );
    let signed_in = common::TestRequest::get(&path)
        .bearer(bob_token.as_str())
        .send(&app)
        .await;
    assert_eq!(
        entity_ids(&signed_in.json_value()),
        ids_of(&[&quote]),
        "a user token sees the quoter"
    );
}
