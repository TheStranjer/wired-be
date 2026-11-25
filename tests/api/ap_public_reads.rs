//! The public-post reads of `GET /objects/{uuid}` and `GET /activities/{uuid}`
//! are decided by `status_visibility::can_view` for the signer the request
//! proves, not by the post's visibility alone: an account the author blocked
//! gets the plain 404 for a `public` post, with `AUTHORIZED_FETCH` on or off,
//! while an unsigned read and an unblocked signer still get the document.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::Utc;
use wired_be::{
    config::Toggle,
    federation::signature::{SignedHeaders, SigningKey, sign_request},
    models::{
        block,
        status::{Status, Visibility},
    },
};

use crate::common;

/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";

/// The two read paths the public-post decision guards for `status`.
fn read_paths(status: &Status) -> [String; 2] {
    [
        format!("/objects/{}", status.id),
        format!("/activities/{}", status.id),
    ]
}

/// Signs a `GET` of `https://test.example{path}` as `signer`.
fn signed_get_as(signer: &common::Signer, path: &str) -> SignedHeaders {
    let key_id = format!("{}#main-key", signer.ap_id);
    sign_request(
        "GET",
        &format!("{BASE}{path}"),
        None,
        &SigningKey {
            key_id: &key_id,
            private_pem: &signer.private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test fetch")
}

/// `GET`s `path` as an `ActivityPub` client, signed when `signed` is given.
async fn fetch(app: &Router, path: &str, signed: Option<&SignedHeaders>) -> common::TestResponse {
    let request = common::TestRequest::get(path).header("accept", "application/activity+json");
    match signed {
        Some(signed) => {
            request
                .header("host", &signed.host)
                .header("date", &signed.date)
                .header("signature", &signed.signature)
                .send(app)
                .await
        }
        None => request.send(app).await,
    }
}

/// A fetch signed by an account the author blocked is the 404 for a public
/// post on both reads, whether `AUTHORIZED_FETCH` is on or off.
#[tokio::test]
async fn a_signed_fetch_by_an_actor_the_author_blocked_is_404() {
    for authorized_fetch in [false, true] {
        let db = common::test_db().await;
        let pool = db.pool().clone();
        let mut config = common::test_config();
        config.fetch.authorized_fetch = Toggle::new(authorized_fetch);
        let alice = common::create_local_user(&pool, "alice").await;
        let carol = common::create_signer(&pool, "carol").await;
        let _blocked = block::block(&pool, BASE, &alice, &carol)
            .await
            .expect("the block write runs");
        let note = common::create_status(&pool, &alice, "public words", Visibility::Public).await;
        let app = common::test_app_with_config(pool, config);

        for path in read_paths(&note) {
            let response = fetch(&app, &path, Some(&signed_get_as(&carol, &path))).await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "the blocked signer's read of {path} (authorized_fetch={authorized_fetch})"
            );
        }
    }
}

/// With the gate off, an unsigned read of a public post is still served on
/// both reads.
#[tokio::test]
async fn an_unsigned_fetch_of_a_public_post_still_succeeds() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "public words", Visibility::Public).await;
    let unlisted = common::create_status(&pool, &alice, "quiet words", Visibility::Unlisted).await;
    let app = common::test_app(pool);

    for post in [&note, &unlisted] {
        for path in read_paths(post) {
            let response = fetch(&app, &path, None).await;
            assert_eq!(
                response.status,
                StatusCode::OK,
                "the unsigned read of {path}"
            );
        }
    }
}

/// A signed fetch by an account the author did not block is served on both
/// reads, with `AUTHORIZED_FETCH` on or off.
#[tokio::test]
async fn a_signed_fetch_by_an_unblocked_actor_of_a_public_post_succeeds() {
    for authorized_fetch in [false, true] {
        let db = common::test_db().await;
        let pool = db.pool().clone();
        let mut config = common::test_config();
        config.fetch.authorized_fetch = Toggle::new(authorized_fetch);
        let alice = common::create_local_user(&pool, "alice").await;
        let carol = common::create_signer(&pool, "carol").await;
        let note = common::create_status(&pool, &alice, "public words", Visibility::Public).await;
        let app = common::test_app_with_config(pool, config);

        for path in read_paths(&note) {
            let response = fetch(&app, &path, Some(&signed_get_as(&carol, &path))).await;
            assert_eq!(
                response.status,
                StatusCode::OK,
                "the unblocked signer's read of {path} (authorized_fetch={authorized_fetch})"
            );
        }
    }
}
