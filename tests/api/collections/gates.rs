//! The gates every collection route shares: 404/410, content
//! negotiation, bad cursors and authorized fetch.

use axum::http::StatusCode;
use chrono::Utc;
use serde_json::json;
use wired_be::{
    config::Toggle,
    federation::signature::{SigningKey, sign_request},
    models::{
        status::Visibility,
        user::{Flag, NewUser},
    },
    services::instance_actor::ensure_instance_actor,
};

use super::support::{ACTIVITY_JSON, BASE, REFUSAL, fetch_ap, signed_get};
use crate::common;

/// Every user-keyed collection route answers like the actor route:
/// unknown and remote accounts are the 404, a deactivated one the 410.
#[tokio::test]
async fn collection_routes_answer_404_and_410_like_the_actor_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let _ex = common::create_local_user_with(&pool, "ex", |new_user| NewUser {
        is_active: Flag::new(false),
        ..new_user
    })
    .await;
    let app = common::test_app(pool);

    let gone_paths = [
        "/users/ex/outbox".to_owned(),
        "/users/ex/followers".to_owned(),
        "/users/ex/following".to_owned(),
        "/users/ex/collections/featured".to_owned(),
    ];
    for path in gone_paths {
        let response = fetch_ap(&app, &path).await;
        common::assert_answer(&response, StatusCode::GONE, &path);
        assert_eq!(
            response.json_value(),
            json!({"error": "Gone"}),
            "{path}: gone, like the actor route"
        );
    }

    let missing_names = ["nobody", bob.nickname.as_str()];
    let roots = [
        "/outbox",
        "/followers",
        "/following",
        "/collections/featured",
    ];
    for name in missing_names {
        for root in roots {
            let path = format!("/users/{name}{root}");
            let plain = fetch_ap(&app, &path).await;
            common::assert_answer(&plain, StatusCode::NOT_FOUND, &path);
            assert_eq!(
                plain.json_value(),
                json!({"error": "Not found"}),
                "{path}: the fallback body, like the actor route"
            );
            assert_eq!(
                plain.header("vary"),
                Some("Accept"),
                "{path}: and the negotiation header"
            );
            let paged = fetch_ap(&app, &format!("{path}?page=true")).await;
            common::assert_answer(&paged, StatusCode::NOT_FOUND, &format!("{path}?page=true"));
        }
    }
}

/// The collection routes negotiate like the actor route: either
/// `ActivityPub` media type gets the document, a browser leaves with the
/// frontend redirect (the profile for account paths, the post page for
/// the object's), and any other `Accept` is the 404.
#[tokio::test]
async fn collections_negotiate_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let app = common::test_app(pool);

    let accepts = [
        "application/activity+json",
        "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
    ];
    for accept in accepts {
        let response = common::TestRequest::get("/users/alice/outbox")
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{accept} speaks ActivityPub"
        );
        assert_eq!(
            response.header("content-type"),
            Some(ACTIVITY_JSON),
            "{accept} gets the ActivityPub media type"
        );
    }

    let browser_asks = [
        ("/users/alice/outbox", format!("{BASE}/alice")),
        ("/users/alice/followers", format!("{BASE}/alice")),
        ("/users/alice/following", format!("{BASE}/alice")),
        ("/users/alice/collections/featured", format!("{BASE}/alice")),
        (
            &format!("/objects/{}/replies", post.id),
            format!("{BASE}/notice/{}", post.id),
        ),
    ];
    for (path, location) in browser_asks {
        let browser = common::TestRequest::get(path)
            .header("accept", "text/html")
            .send(&app)
            .await;
        assert_eq!(
            browser.status,
            StatusCode::FOUND,
            "{path} sends the browser on"
        );
        assert_eq!(
            browser.header("location"),
            Some(location.as_str()),
            "{path}"
        );
        assert_eq!(browser.header("vary"), Some("Accept"), "{path} varies");
    }

    let strangers = ["*/*", "application/json", "text/plain"];
    for accept in strangers {
        let response = common::TestRequest::get("/users/alice/following")
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{accept} is no ActivityPub"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "the fallback body for {accept}"
        );
    }
}

/// A cursor that is no id is the shared 400, on every paged route.
#[tokio::test]
async fn bad_cursors_are_the_shared_invalid_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let app = common::test_app(pool);

    let paged = [
        "/users/alice/outbox".to_owned(),
        "/users/alice/followers".to_owned(),
        "/users/alice/following".to_owned(),
        format!("/objects/{}/replies", post.id),
    ];
    for path in paged {
        let response = fetch_ap(&app, &format!("{path}?page=true&max_id=nonsense")).await;
        let who = format!("{path} with a bad cursor");
        common::assert_answer(&response, StatusCode::BAD_REQUEST, &who);
        assert_eq!(
            response.json_value(),
            json!({"error": "Invalid id"}),
            "{who}: the shared pagination refusal"
        );
    }
}

/// With `AUTHORIZED_FETCH` on, every `ActivityPub` read route demands a
/// valid HTTP Signature first: the unsigned request is the 401, and a
/// signature made over the exact path (query included) is served.
#[tokio::test]
async fn authorized_fetch_gates_every_collection_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let mut config = common::test_config();
    config.fetch.authorized_fetch = Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config.clone());

    let gated = [
        "/users/alice/outbox".to_owned(),
        "/users/alice/outbox?page=true".to_owned(),
        "/users/alice/followers".to_owned(),
        "/users/alice/following".to_owned(),
        "/users/alice/collections/featured".to_owned(),
        format!("/objects/{}/replies", post.id),
    ];
    for path in gated {
        let response = fetch_ap(&app, &path).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path} is gated");
        assert_eq!(response.json_value()["error"], REFUSAL, "{path} says why");
    }

    let actor = ensure_instance_actor(&pool, &config)
        .await
        .expect("the instance actor is provisioned");
    let key_id = format!("{}#main-key", actor.ap_id);
    let opened = [
        "/users/alice/outbox?page=true".to_owned(),
        "/users/alice/collections/featured".to_owned(),
        format!("/objects/{}/replies", post.id),
    ];
    for path in opened {
        let signed = sign_request(
            "GET",
            &format!("{BASE}{path}"),
            None,
            &SigningKey {
                key_id: &key_id,
                private_pem: actor
                    .private_key
                    .as_deref()
                    .expect("the provisioned actor holds its private key"),
            },
            Utc::now(),
        )
        .expect("signing the test fetch");
        let response = signed_get(&app, &path, &signed).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path} opens to its signature"
        );
    }
}
