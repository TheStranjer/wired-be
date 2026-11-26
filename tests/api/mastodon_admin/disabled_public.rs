//! A disabled account only loses its owner's sign-in (WBE-883): the profile,
//! its posts and its federation stay public, to anonymous readers and to
//! other accounts alike, while the owner's token and password are refused.

use axum::{Router, http::StatusCode};
use serde_json::json;
use uuid::Uuid;
use wired_be::models::status::Visibility;

use super::write_support::{
    DISABLED, Writes, account_action_path, act, disabled_state, login, post_json, seed_writes,
    verify,
};
use crate::common::{self, TestRequest};

/// Who reads: nobody signed in, or the bearer token of another account.
fn readers(viewer: &str) -> [(&'static str, Option<&str>); 2] {
    [("anonymous", None), ("another account", Some(viewer))]
}

/// Sends `GET path` as `token` (anonymous for `None`), asking for
/// `ActivityPub` JSON when `activity` is set.
async fn get_as(
    app: &Router,
    path: &str,
    token: Option<&str>,
    activity: bool,
) -> common::TestResponse {
    let mut request = TestRequest::get(path);
    if activity {
        request = request.header("accept", "application/activity+json");
    }
    if let Some(token) = token {
        request = request.bearer(token);
    }
    request.send(app).await
}

/// Disables `plain` through the umbrella and returns its public post's id.
async fn disabled_with_a_post(seeded: &Writes) -> Uuid {
    let post = common::create_status(
        &seeded.pool,
        &seeded.plain,
        "still public",
        Visibility::Public,
    )
    .await
    .id;
    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({"type": "disable"}),
    )
    .await;
    assert_eq!(
        disabled_state(&seeded.pool, seeded.plain.id).await,
        Some(true),
        "the account is disabled"
    );
    post
}

/// The local API still shows a disabled account, its lookup and its posts.
#[tokio::test]
async fn a_disabled_account_and_its_posts_stay_visible_over_the_api() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let viewer = common::create_app_and_token(&seeded.pool, &seeded.beeper, "read").await;
    let _post = disabled_with_a_post(&seeded).await;
    let id = seeded.plain.id;

    for (who, token) in readers(&viewer) {
        let shown = get_as(&seeded.app, &format!("/api/v1/accounts/{id}"), token, false).await;
        common::assert_answer(&shown, StatusCode::OK, &format!("the account for {who}"));
        assert_eq!(shown.json_value()["id"], json!(id.to_string()), "{who}");
        assert_eq!(
            shown.json_value()["suspended"],
            json!(false),
            "a disable is no suspension for {who}"
        );
        let found = get_as(
            &seeded.app,
            "/api/v1/accounts/lookup?acct=plain",
            token,
            false,
        )
        .await;
        common::assert_answer(&found, StatusCode::OK, &format!("the lookup for {who}"));
        assert_eq!(found.json_value()["id"], json!(id.to_string()), "{who}");
        let posts = get_as(
            &seeded.app,
            &format!("/api/v1/accounts/{id}/statuses"),
            token,
            false,
        )
        .await;
        common::assert_answer(&posts, StatusCode::OK, &format!("the statuses for {who}"));
        assert_eq!(
            posts.json_value().as_array().map(Vec::len),
            Some(1),
            "the post is listed for {who}"
        );
    }
}

/// Over `ActivityPub` the actor, its note and its featured collection keep
/// answering, and the actor does not say `suspended`.
#[tokio::test]
async fn a_disabled_account_keeps_federating_its_documents() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let post = disabled_with_a_post(&seeded).await;

    let actor = get_as(&seeded.app, "/users/plain", None, true).await;
    common::assert_answer(&actor, StatusCode::OK, "the actor document");
    assert!(
        actor.json_value().get("suspended").is_none(),
        "nothing tells a peer the account is gone"
    );
    let note = get_as(&seeded.app, &format!("/objects/{post}"), None, true).await;
    common::assert_answer(&note, StatusCode::OK, "the note");
    let featured = get_as(&seeded.app, "/users/plain/collections/featured", None, true).await;
    common::assert_answer(&featured, StatusCode::OK, "the featured collection");
}

/// The owner's token answers 403 and the password grant fails, and `enable`
/// gives both back.
#[tokio::test]
async fn a_disabled_owner_is_locked_out_until_enabled() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let owner = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;
    let client = common::create_app(&seeded.pool, "read").await;
    let _post = disabled_with_a_post(&seeded).await;

    let refused = verify(&seeded.app, &owner).await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "the owner's token");
    common::assert_error(&refused, DISABLED, "is the disabled refusal");
    let grant = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "plain",
    )
    .await;
    common::assert_answer(&grant, StatusCode::FORBIDDEN, "the password grant");
    assert_eq!(
        grant.json_value()["identifier"],
        json!("account_is_disabled"),
        "with the grant's own reason code"
    );

    let enabled = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.plain.id, "enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(&enabled, StatusCode::OK, "the enable");
    let allowed = verify(&seeded.app, &owner).await;
    common::assert_answer(&allowed, StatusCode::OK, "the token works again");
}
