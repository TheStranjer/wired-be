//! Tests for the `AUTHORIZED_FETCH` gate on the `ActivityPub` actor route
//! (`src/federation/authorized_fetch.rs`, wired in `src/api/activitypub.rs`).
//!
//! While the flag is off — the default — actor documents answer anonymous
//! `ActivityPub` requests exactly as before. When it is on, a `GET` of
//! `/users/{nickname}` must carry an HTTP Signature that verifies against
//! the signing actor's stored key: unsigned or unverifiable requests are
//! refused with 401, while `GET /internal/fetch` — the document a peer
//! reads to learn the key it signs with — stays open.
//!
//! The verification itself (every accept/reject corner of the signature
//! codec) is covered where it lives, in
//! `src/federation/signature/verify.rs` and
//! `src/federation/authorized_fetch.rs`; what these tests own is the
//! route: header plumbing, statuses and bodies over the real router.

#![cfg(test)]

use axum::http::StatusCode;
use chrono::Utc;
use wired_be::{
    config::{Config, Toggle},
    federation::signature::{SignedHeaders, SigningKey, sign_request},
    services::instance_actor::{INSTANCE_ACTOR_PATH, ensure_instance_actor},
};

use crate::common;

/// The refusal body every unsatisfied gate check answers with.
const REFUSAL: &str = "This instance requires authenticated fetches";

/// [`common::test_config`] with `AUTHORIZED_FETCH` switched to `on`.
fn gated(on: bool) -> Config {
    let mut config = common::test_config();
    config.authorized_fetch = Toggle::new(on);
    config
}

/// A signed `GET` of `https://test.example/users/alice` under the instance
/// actor's key, as the headers a peer would send. The actor is provisioned
/// through the same lazy entry point the routes use.
async fn signed_as_instance_actor(pool: &sqlx::PgPool, config: &Config) -> SignedHeaders {
    let actor = ensure_instance_actor(pool, config)
        .await
        .expect("the instance actor is provisioned");
    let key_id = format!("{}#main-key", actor.ap_id);
    sign_request(
        "GET",
        "https://test.example/users/alice",
        None,
        &SigningKey {
            key_id: &key_id,
            private_pem: actor.private_key.as_deref().expect("it has a private key"),
        },
        Utc::now(),
    )
    .expect("signing the test fetch")
}

/// `GET /users/alice` as an `ActivityPub` client, with `signed`'s headers
/// added when there is a signature to present.
async fn fetch_alice(app: &axum::Router, signed: Option<&SignedHeaders>) -> common::TestResponse {
    let mut request =
        common::TestRequest::get("/users/alice").header("accept", "application/activity+json");
    if let Some(signed) = signed {
        request = request
            .header("host", &signed.host)
            .header("date", &signed.date)
            .header("signature", &signed.signature);
    }
    request.send(app).await
}

/// With the gate off (the default), anonymous `ActivityPub` fetches keep
/// answering — nothing about the actor route changes — and a signature
/// is never demanded.
#[tokio::test]
async fn unsigned_fetches_answer_while_the_gate_is_off() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app_with_config(pool.clone(), gated(false));

    let response = fetch_alice(&app, None).await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "the flag-off default is the pre-gate behaviour"
    );
    assert_eq!(
        response.json_value()["id"],
        "https://test.example/users/alice",
        "alice's document, no signature asked"
    );
}

/// With the gate on: the unsigned fetch is refused 401, a signature made
/// over a different path does not unlock the request it is sent with, and
/// a signature the stored instance actor key verifies does. The internal
/// fetch document itself stays readable — a peer must be able to learn
/// the key before it can sign with it.
#[tokio::test]
async fn the_gate_demands_a_verifiable_signature() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let config = gated(true);
    let app = common::test_app_with_config(pool.clone(), config.clone());

    let unsigned = fetch_alice(&app, None).await;
    assert_eq!(
        unsigned.status,
        StatusCode::UNAUTHORIZED,
        "no Signature header cannot be verified"
    );
    assert_eq!(
        unsigned.json_value()["error"],
        REFUSAL,
        "the refusal body follows decision 8"
    );

    let signed = signed_as_instance_actor(&pool, &config).await;
    let wrong_path = common::TestRequest::get("/users/mallory")
        .header("accept", "application/activity+json")
        .header("host", &signed.host)
        .header("date", &signed.date)
        .header("signature", &signed.signature)
        .send(&app)
        .await;
    assert_eq!(
        wrong_path.status,
        StatusCode::UNAUTHORIZED,
        "alice's signature does not authorize a fetch of mallory"
    );

    let good = fetch_alice(&app, Some(&signed)).await;
    assert_eq!(
        good.status,
        StatusCode::OK,
        "a signature the stored key verifies unlocks alice"
    );
    assert_eq!(
        good.json_value()["id"],
        "https://test.example/users/alice",
        "and the document is alice's"
    );

    let discovery = common::TestRequest::get(INSTANCE_ACTOR_PATH)
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    assert_eq!(
        discovery.status,
        StatusCode::OK,
        "the key-discovery document is never gated"
    );

    let browser = common::TestRequest::get("/users/alice")
        .header("accept", "text/html")
        .send(&app)
        .await;
    assert_eq!(
        browser.status,
        StatusCode::FOUND,
        "content negotiation still answers before the gate does, and the \
         frontend redirect needs no signature"
    );
    assert_eq!(
        browser.header("location"),
        Some("https://test.example/alice"),
        "the browser is sent to the frontend profile page"
    );
}
