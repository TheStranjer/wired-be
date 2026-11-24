//! The callback's refusals and their edges: every provider failure
//! bounced to the app as `access_denied`, the client `state` riding the
//! failure redirect, the deactivated linked user and the deleted
//! application, the SSRF guard on the provider calls, dotted field names
//! over a nested document, the sibling `/oauth` routes the provider
//! segment leaves alone, and the remote actor whose link is no login.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::user_identity;

use super::provider::{LINKED_UID, MockProvider, PROVIDER, REDIRECT, UNLINKED_UID};
use super::support::{
    assert_no_store, count_codes, fixture, link, location, prepare, query_param, stored_params,
};
use crate::common;

/// Every provider failure — its own `error` parameter, no code, a refused
/// token exchange, an answer with no token, a userinfo read that failed or
/// answered no JSON, and a document naming no account id — is bounced back to
/// the app's redirect URI as `error=access_denied`, with no code minted.
#[tokio::test]
async fn the_callback_bounces_a_provider_failure_to_the_app() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "kate", "read write").await;
    link(&pool, &site.user, LINKED_UID).await;

    for (query, why) in [
        (String::new(), "no code at all"),
        (
            format!("error=access_denied&code={LINKED_UID}"),
            "the provider's own refusal",
        ),
        (
            "code=server-error".to_owned(),
            "a token endpoint that failed",
        ),
        ("code=no-token".to_owned(), "an answer with no access_token"),
        (
            "code=forbidden".to_owned(),
            "a userinfo read that was refused",
        ),
        ("code=html".to_owned(), "a userinfo answer that is not JSON"),
        ("code=no-uid".to_owned(), "a document naming no account id"),
    ] {
        let state = prepare(&site.http, &site.app, "").await;
        let response = common::TestRequest::get(&format!(
            "/oauth/{PROVIDER}/callback?state={state}{}",
            if query.is_empty() {
                String::new()
            } else {
                format!("&{query}")
            }
        ))
        .send(&site.http)
        .await;
        assert_eq!(response.status, StatusCode::FOUND, "{why} redirects");
        assert_no_store(&response);
        assert_eq!(
            location(&response),
            format!("{REDIRECT}?error=access_denied"),
            "the app is told access was denied, for {why}"
        );
    }
    assert_eq!(
        count_codes(&pool).await,
        0,
        "none of the failures minted a code"
    );
    site.provider.stop();
}

/// The client's `state` rides the failure redirect too, so the app can tie
/// the refusal back to the login it started.
#[tokio::test]
async fn the_provider_failure_echoes_the_client_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "liam", "read").await;
    let state = prepare(&site.http, &site.app, "&state=client-state").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?error=access_denied&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "the refusal redirects");
    assert_eq!(
        location(&response),
        format!("{REDIRECT}?error=access_denied&state=client-state"),
        "with the state the app sent to prepare_request"
    );
    site.provider.stop();
}

/// A linked account that is deactivated is refused its login the way the
/// token endpoint refuses it — 403 with Pleroma's machine-readable reason,
/// not a code and not a registration form.
#[tokio::test]
async fn the_callback_refuses_a_deactivated_linked_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "mallory", "read").await;
    let state = prepare(&site.http, &site.app, "").await;
    // The account is suspended after the round trip was prepared: the login
    // gate runs on the callback, not on prepare_request.
    let suspended = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(site.user.id)
        .execute(&pool)
        .await
        .expect("the suspension runs");
    assert_eq!(suspended.rows_affected(), 1, "the fixture was suspended");
    link(&pool, &site.user, LINKED_UID).await;

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "disabled is no login"
    );
    assert_eq!(
        response.json_value()["error"],
        json!("Your account is currently disabled"),
        "Pleroma's wording: {}",
        response.text()
    );
    assert_eq!(
        response.json_value()["identifier"],
        json!("account_is_disabled"),
        "and its reason code"
    );
    assert_no_store(&response);
    assert_eq!(count_codes(&pool).await, 0, "the refusal minted no code");
    site.provider.stop();
}

/// A login prepared for an application that no longer exists cannot be
/// finished: the same 400 the consent endpoint gives an unknown client, and
/// no code is minted.
#[tokio::test]
async fn the_callback_refuses_a_deleted_application() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "nina", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;
    let gone = sqlx::query("DELETE FROM oauth_apps WHERE id = $1")
        .bind(site.app.id)
        .execute(&pool)
        .await
        .expect("the app fixture is deleted");
    assert_eq!(gone.rows_affected(), 1, "one application went away");

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "no application to mint for"
    );
    assert_eq!(response.error_text(), "Invalid client", "named as such");
    assert_no_store(&response);
    site.provider.stop();
}

/// Both provider calls go through the SSRF-guarded client: with the guard on,
/// a provider on loopback is unreachable, and the callback bounces the
/// app's login back as denied rather than fetching it.
#[tokio::test]
async fn the_provider_calls_pass_the_ssrf_guard() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "omar", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;

    let mut guarded = site.provider.config();
    guarded.fetch.http_allow_private_hosts = false;
    let guarded_http = common::test_app_with_config(pool.clone(), guarded);
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&guarded_http)
    .await;

    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "a blocked provider is a provider failure"
    );
    assert_eq!(
        location(&response),
        format!("{REDIRECT}?error=access_denied"),
        "and the app is told access was denied"
    );
    assert_eq!(
        count_codes(&pool).await,
        0,
        "nothing was minted on a blocked exchange"
    );
    site.provider.stop();
}

/// The configured field names are dotted paths into the provider's document:
/// `nested.id` reads the uid from the nested object, `nested.missing` says
/// the provider does not tell us an email, and the account behind the
/// nested id is logged in.
#[tokio::test]
async fn dotted_field_names_read_a_nested_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "pam", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let http = common::test_app_with_config(
        pool.clone(),
        MockProvider::config_with(vec![site.provider.nested_provider(PROVIDER)]),
    );

    let state = prepare(&http, &site.app, "").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "the nested uid identifies the linked account"
    );
    assert!(
        query_param(&location(&response), "code").is_some(),
        "with a code: {}",
        location(&response)
    );

    // The same nested read on an unlinked account hands the registration
    // form what the provider did say, and a null for what it did not.
    let stranger = prepare(&http, &site.app, "").await;
    let unlinked = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={stranger}"
    ))
    .send(&http)
    .await;
    assert_eq!(
        location(&unlinked),
        format!("https://fe.example/oauth/external/register?state={stranger}"),
        "the nested-field hand-off goes to the same screen"
    );
    let (_, nested) = stored_params(&pool, &stranger).await;
    assert_eq!(
        nested.get("pending"),
        Some(&json!({
            "provider": PROVIDER,
            "uid": UNLINKED_UID,
            "nickname": format!("user-{UNLINKED_UID}"),
            "email": null,
            "name": format!("User {UNLINKED_UID}"),
        })),
        "the optional fields are dug out the same way: {nested}"
    );
    site.provider.stop();
}

/// `GET /oauth/authorize` and the token endpoints keep answering their own
/// paths: the `{provider}` segment must not swallow them.
#[tokio::test]
async fn the_provider_route_leaves_the_sibling_paths_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "quinn", "read").await;
    let consent = common::TestRequest::get("/oauth/authorize?response_type=token")
        .send(&site.http)
        .await;
    assert_eq!(
        consent.status,
        StatusCode::BAD_REQUEST,
        "the consent endpoint still runs its own flow check"
    );
    assert_eq!(
        consent.error_text(),
        "unsupported_response_type",
        "with its own body"
    );

    let token = common::TestRequest::post("/oauth/token")
        .json(&json!({"grant_type": "password"}))
        .send(&site.http)
        .await;
    assert_eq!(
        token.status,
        StatusCode::BAD_REQUEST,
        "the token endpoint answers"
    );
    assert_eq!(
        token.error_text(),
        "Invalid credentials",
        "with its own body"
    );
    site.provider.stop();
}

/// A user's own row is what the identity link points at: the model write the
/// endpoint reads through is the one the fixture used.
#[tokio::test]
async fn a_link_to_a_remote_actor_is_not_a_login() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "rosa", "read").await;
    let remote = common::create_remote_user(&pool, "octocat", "remote.example").await;
    let made = user_identity::link(&pool, remote.id, PROVIDER, LINKED_UID)
        .await
        .expect("the identity fixture links a remote row");
    assert_eq!(made.user_id, remote.id, "the link names the remote actor");

    let state = prepare(&site.http, &site.app, "").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(
        response.status,
        StatusCode::FOUND,
        "the remote link reads as unlinked"
    );
    assert_eq!(
        location(&response),
        format!("https://fe.example/oauth/external/register?state={state}"),
        "so the account goes to the registration screen"
    );
    assert_eq!(
        count_codes(&pool).await,
        0,
        "and no code lets a remote actor log in"
    );
    site.provider.stop();
}
