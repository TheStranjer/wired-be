//! The aliases routes over HTTP: adding, listing and removing aliases and
//! the refusals for unresolvable targets and bodies without the parameter.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{federation::publisher, models::user::User};

use super::support::{
    ALIASES, UNRESOLVABLE, UNSEEN_URL, delete_alias, get_aliases, put_alias, queued_jobs, stored,
};
use crate::common;

/// The aliases lifecycle over a mirror-backed alias, a local-account
/// alias and a raw URL this instance cannot resolve to a row.
#[tokio::test]
async fn add_list_and_remove_aliases() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.also_known_as = vec![UNSEEN_URL.to_owned()];
        new
    })
    .await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    let app = common::test_app(pool.clone());

    let listed = get_aliases(&app, &token).await;
    assert_eq!(listed.status, StatusCode::OK, "the owner may list");
    assert_eq!(
        listed.json_value(),
        json!({"aliases": [UNSEEN_URL]}),
        "an id with no row falls back to the raw URL"
    );

    let added = put_alias(&app, &token, "bob@remote.example").await;
    assert_eq!(
        added.json_value(),
        json!({"status": "success"}),
        "adding a mirrored remote handle succeeds"
    );
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as,
        vec![UNSEEN_URL.to_owned(), bob.ap_id.clone()],
        "the target's actor id is appended, in order"
    );

    let again = put_alias(&app, &token, "bob@remote.example").await;
    assert_eq!(again.status, StatusCode::OK, "adding twice succeeds");
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as.len(),
        2,
        "and appends only once"
    );

    let by_url = put_alias(&app, &token, &erin.ap_id).await;
    assert_eq!(
        by_url.status,
        StatusCode::OK,
        "an alias given as an ActivityPub id resolves too"
    );

    let listing = get_aliases(&app, &token).await.json_value();
    assert_eq!(
        listing,
        json!({"aliases": [UNSEEN_URL, "bob@remote.example", "erin@test.example"]}),
        "mirrored aliases render as full acct handles (Pleroma's \
         User.full_nickname), unknown ids stay raw URLs"
    );

    let removed = delete_alias(&app, &token, "erin@test.example").await;
    assert_eq!(
        removed.json_value(),
        json!({"status": "success"}),
        "removal by the handle the list rendered"
    );
    let also = delete_alias(&app, &token, UNSEEN_URL).await;
    assert_eq!(also.status, StatusCode::OK, "removal by the raw URL");
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as,
        vec![bob.ap_id.clone()],
        "only the untouched alias remains"
    );

    let nothing = delete_alias(&app, &token, UNRESOLVABLE).await;
    assert_eq!(
        nothing.json_value(),
        json!({"status": "success"}),
        "removing an alias that was never there is a no-op success"
    );
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as,
        vec![bob.ap_id.clone()],
        "and changed nothing"
    );
}

/// A target neither the database nor a fast-failing `WebFinger` check
/// can answer is the 404 the ticket fixes; nothing is appended.
#[tokio::test]
async fn an_unresolvable_alias_target_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    for alias in [UNRESOLVABLE, "nobody"] {
        let response = put_alias(&app, &token, alias).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{alias}` resolves to no account: 404"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Record not found"}),
            "the generic not-found body, `{alias}`"
        );
    }
    assert!(
        stored(&pool, alice.id).await.also_known_as.is_empty(),
        "the refusals appended nothing"
    );
}

/// An `alias` the body does not carry is the extractor's 422.
#[tokio::test]
async fn an_alias_body_without_the_parameter_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());
    let response = common::TestRequest::put(ALIASES)
        .bearer(&token)
        .json(&json!({}))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the rejection is JSON"
    );
}

/// Alice's local row with a followers collection, so her profile `Update`
/// has an audience and is queued.
async fn publishing_alice(pool: &PgPool) -> User {
    common::create_local_user_with(pool, "alice", |mut new| {
        new.followers_url = Some(format!("{}/followers", new.ap_id));
        new
    })
    .await
}

/// The `alsoKnownAs` of every queued profile `Update`, oldest first.
async fn queued_alias_lists(pool: &PgPool) -> Vec<Value> {
    queued_jobs(pool, publisher::KIND_PUBLISH)
        .await
        .into_iter()
        .map(|(_queue, args)| {
            assert_eq!(args["activity"]["type"], "Update", "a profile Update");
            args["activity"]["object"]["alsoKnownAs"].clone()
        })
        .collect()
}

/// Adding and removing an alias each federate the actor `Update` with the
/// list as it now stands; a repeated add changes nothing and queues
/// nothing.
#[tokio::test]
async fn alias_changes_queue_an_actor_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = publishing_alice(&pool).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let added = put_alias(&app, &token, &bob.ap_id).await;
    assert_eq!(added.status, StatusCode::OK, "the alias is added");
    assert_eq!(
        queued_alias_lists(&pool).await,
        vec![json!([bob.ap_id])],
        "one Update carrying the new alias"
    );

    let _cleared = sqlx::query("DELETE FROM jobs WHERE kind = $1")
        .bind(publisher::KIND_PUBLISH)
        .execute(&pool)
        .await
        .expect("the queue is emptied so the debounce key is free");
    let again = put_alias(&app, &token, &bob.ap_id).await;
    assert_eq!(again.status, StatusCode::OK, "adding twice succeeds");
    assert!(
        queued_alias_lists(&pool).await.is_empty(),
        "an unchanged list announces nothing"
    );

    let removed = delete_alias(&app, &token, &bob.ap_id).await;
    assert_eq!(removed.status, StatusCode::OK, "the alias is dropped");
    assert_eq!(
        queued_alias_lists(&pool).await,
        vec![json!([])],
        "one Update with the alias gone"
    );

    let _cleared_again = sqlx::query("DELETE FROM jobs WHERE kind = $1")
        .bind(publisher::KIND_PUBLISH)
        .execute(&pool)
        .await
        .expect("the queue is emptied");
    let nothing = delete_alias(&app, &token, &bob.ap_id).await;
    assert_eq!(nothing.status, StatusCode::OK, "removing nothing succeeds");
    assert!(
        queued_alias_lists(&pool).await.is_empty(),
        "an unchanged list announces nothing on removal either"
    );
}
