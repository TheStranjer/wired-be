//! Split-domain support (`LOCAL_DOMAIN`, WBE-889): handles show one domain
//! (`@alice@whatever.com`) while the instance is served from another
//! (`BASE_URL=https://test.example`).
//!
//! These tests run the real router on a config whose handle domain is
//! `whatever.com`: `WebFinger` names the handle domain in its subject and
//! accepts both domains as resources, the instance documents report the
//! handle domain while the streaming URL stays on the web host, account
//! entities and acct lookups use the handle domain, and every URL stays on
//! the web host.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;

use crate::common;

/// The actor id of the fixture `alice`: always on the web host.
const ALICE_AP_ID: &str = "https://test.example/users/alice";
/// The handle domain of the split config.
const HANDLE_DOMAIN: &str = "whatever.com";

/// The real router on a config whose handle domain is [`HANDLE_DOMAIN`].
fn split_app(pool: &PgPool) -> Router {
    let mut config = common::test_config();
    config.local_domain = HANDLE_DOMAIN.to_owned();
    common::test_app_with_config(pool.clone(), config)
}

/// `GET /.well-known/webfinger?resource=<resource>` from `app`.
async fn finger(app: &Router, resource: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/.well-known/webfinger?resource={resource}"))
        .send(app)
        .await
}

/// `GET path` from `app`, expecting 200 and a JSON body.
async fn get_json(app: &Router, path: &str) -> Value {
    let response = common::TestRequest::get(path).send(app).await;
    common::assert_answer(&response, StatusCode::OK, path);
    response.json_value()
}

/// Both acct forms on either domain and the actor URL resolve the user,
/// and every answer names the handle domain in its subject while the
/// aliases and the `self` link stay on the web host.
#[tokio::test]
async fn webfinger_names_the_handle_domain_and_keeps_the_links_on_the_web_host() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = split_app(&pool);

    for resource in [
        "acct:alice@whatever.com",
        "alice@whatever.com",
        "acct:alice@test.example",
        "alice@test.example",
        "https%3A%2F%2Ftest.example%2Fusers%2Falice",
    ] {
        let response = finger(&app, resource).await;
        common::assert_answer(&response, StatusCode::OK, resource);
        let document = response.json_value();
        assert_eq!(
            document["subject"], "acct:alice@whatever.com",
            "{resource} answers with the handle domain"
        );
        assert_eq!(
            document["aliases"][0], ALICE_AP_ID,
            "{resource}: the alias is the actor URL on the web host"
        );
        let own_link = document["links"]
            .as_array()
            .and_then(|links| links.iter().find(|link| link["rel"] == "self"))
            .expect("the document carries a self link");
        assert_eq!(
            own_link["href"], ALICE_AP_ID,
            "{resource}: the self link points at the actor on the web host"
        );
    }
}

/// Matching is case-insensitive on both domains, and a foreign domain is
/// still a 404.
#[tokio::test]
async fn webfinger_matches_case_insensitively_and_refuses_foreign_domains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = split_app(&pool);

    for resource in [
        "acct:alice@WHATEVER.COM",
        "acct:ALICE@Whatever.Com",
        "acct:alice@TEST.EXAMPLE",
    ] {
        let response = finger(&app, resource).await;
        common::assert_answer(&response, StatusCode::OK, resource);
        assert_eq!(
            response.json_value()["subject"],
            "acct:alice@whatever.com",
            "{resource} still finds alice"
        );
    }
    for resource in [
        "acct:alice@other.example",
        "alice@other.example",
        "acct:alice@whatever.com.evil.example",
        "acct:nobody@whatever.com",
    ] {
        let response = finger(&app, resource).await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, resource);
    }
}

/// Without `LOCAL_DOMAIN` the handle domain is the web host: the handle
/// domain of the split tests is a foreign domain, and the document names
/// the web host.
#[tokio::test]
async fn an_unsplit_config_is_unchanged() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let response = finger(&app, "acct:alice@test.example").await;
    common::assert_answer(&response, StatusCode::OK, "the web host");
    assert_eq!(
        response.json_value()["subject"],
        "acct:alice@test.example",
        "the subject names the web host"
    );
    let foreign = finger(&app, "acct:alice@whatever.com").await;
    common::assert_answer(&foreign, StatusCode::NOT_FOUND, "an unconfigured domain");
}

/// host-meta keeps pointing at the web host's `WebFinger`: the document is
/// the same with and without the split.
#[tokio::test]
async fn host_meta_is_the_same_with_and_without_the_split() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let split = split_app(&pool);
    let plain = common::test_app(pool.clone());

    let split_answer = common::TestRequest::get("/.well-known/host-meta")
        .send(&split)
        .await;
    let plain_answer = common::TestRequest::get("/.well-known/host-meta")
        .send(&plain)
        .await;
    common::assert_answer(&split_answer, StatusCode::OK, "split host-meta");
    assert_eq!(
        split_answer.text(),
        plain_answer.text(),
        "host-meta does not change"
    );
}

/// The v1 `uri` and the v2 `domain` report the handle domain; the
/// streaming URLs stay on the web host.
#[tokio::test]
async fn the_instance_documents_report_the_handle_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = split_app(&pool);

    let v1 = get_json(&app, "/api/v1/instance").await;
    assert_eq!(v1["uri"], HANDLE_DOMAIN, "v1 uri is the handle domain");
    assert_eq!(
        v1["urls"]["streaming_api"], "wss://test.example",
        "v1 streaming stays on the web host"
    );
    let v2 = get_json(&app, "/api/v2/instance").await;
    assert_eq!(
        v2["domain"], HANDLE_DOMAIN,
        "v2 domain is the handle domain"
    );
    assert_eq!(
        v2["configuration"]["urls"]["streaming"], "wss://test.example",
        "v2 streaming stays on the web host"
    );

    let plain = common::test_app(pool.clone());
    assert_eq!(
        get_json(&plain, "/api/v1/instance").await["uri"],
        "test.example",
        "unsplit v1 uri is the web host"
    );
    assert_eq!(
        get_json(&plain, "/api/v2/instance").await["domain"],
        "test.example",
        "unsplit v2 domain is the web host"
    );
}

/// An account entity's `fqn` uses the handle domain, its `acct` stays the
/// bare nickname, and its URLs stay on the web host; either domain finds
/// the account by handle.
#[tokio::test]
async fn accounts_show_the_handle_domain_and_resolve_by_either() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = split_app(&pool);

    for acct in [
        "alice",
        "alice@whatever.com",
        "ALICE@Whatever.COM",
        "alice@test.example",
    ] {
        let account = get_json(&app, &format!("/api/v1/accounts/lookup?acct={acct}")).await;
        assert_eq!(
            account["id"],
            alice.id.to_string(),
            "`{acct}` finds the local alice"
        );
        assert_eq!(
            account["fqn"], "alice@whatever.com",
            "`{acct}`: the fqn uses the handle domain"
        );
        assert_eq!(account["acct"], "alice", "`{acct}`: acct stays bare");
        assert_eq!(
            account["url"]
                .as_str()
                .unwrap_or_default()
                .split('/')
                .nth(2),
            Some("test.example"),
            "`{acct}`: the profile URL stays on the web host"
        );
    }
    let missing = common::TestRequest::get("/api/v1/accounts/lookup?acct=nobody@whatever.com")
        .send(&app)
        .await;
    common::assert_answer(
        &missing,
        StatusCode::NOT_FOUND,
        "an unknown handle-domain acct",
    );
}
