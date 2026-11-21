//! The dismissal routes: `POST /api/v1/notifications/clear`, the single
//! `POST /api/v1/notifications/{id}/dismiss` and its legacy body form, and
//! `DELETE /api/v1/notifications/destroy_multiple` — the rows stay,
//! stamped, and vanish from every read; foreign, unknown and malformed ids
//! are refused.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::support::{
    NOT_FOUND, READ_WRITE, V1, assert_empty_object, fixture, listed_keys, post_json, seed_pair,
    stored_row, ungrouped,
};
use crate::common;

/// `POST /api/v1/notifications/clear` dismisses every notification of the
/// caller — the rows stay, stamped, and vanish from the listing — and
/// answers `{}`; a second clear still answers `{}`.
#[tokio::test]
async fn clear_dismisses_every_notification_of_the_caller() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&response, "the clear");
    assert!(
        listed_keys(&app, &seed.token).await.is_empty(),
        "dismissed notifications vanish from the listing"
    );
    for id in [seed.favourite.id, seed.follow.id] {
        let row = stored_row(&pool, id).await.expect("the row stays");
        assert!(row.dismissed_at.is_some(), "{id} is stamped, not deleted");
    }

    let repeat = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&repeat, "a second clear");
}

/// Clear touches nobody else's rows: another account's notification stays
/// undismissed.
#[tokio::test]
async fn clear_is_scoped_to_the_caller() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let daves_note = common::create_notification(&pool, &dave, &mallory, "follow", None).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&response, "alice's clear");
    let foreign = stored_row(&pool, daves_note.id)
        .await
        .expect("the foreign row stays");
    assert!(foreign.dismissed_at.is_none(), "and stays undismissed");
}

/// The path dismissal removes exactly the notification named, keeps the
/// row soft-dismissed, and answers `{}` again for a repeat.
#[tokio::test]
async fn dismiss_removes_one_notification_from_the_listing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&format!("{V1}/{}/dismiss", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&response, "the path dismissal");
    assert_eq!(
        listed_keys(&app, &seed.token).await,
        vec![ungrouped(&seed.follow)],
        "only the named row left the listing"
    );
    let row = stored_row(&pool, seed.favourite.id)
        .await
        .expect("the row stays");
    assert!(row.dismissed_at.is_some(), "dismissal is soft");

    let repeat = common::TestRequest::post(&format!("{V1}/{}/dismiss", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_empty_object(&repeat, "a repeat dismissal of an owned row");
}

/// A foreign, unknown or malformed id is the same 404 on both dismissal
/// shapes — the route never says whether somebody else holds that id.
#[tokio::test]
async fn dismissal_refuses_foreign_unknown_and_malformed_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let foreign = common::create_notification(&pool, &dave, &mallory, "follow", None).await;
    let unknown = Uuid::now_v7();
    let app = common::test_app(pool);

    for id in [foreign.id, unknown, Uuid::nil()] {
        let by_path = common::TestRequest::post(&format!("{V1}/{id}/dismiss"))
            .bearer(&seed.token)
            .send(&app)
            .await;
        common::assert_answer(
            &by_path,
            StatusCode::NOT_FOUND,
            &format!("dismissal of `{id}`"),
        );
        common::assert_error(&by_path, NOT_FOUND, &format!("dismissal of `{id}`"));
        let by_body = post_json(
            &app,
            &format!("{V1}/dismiss"),
            &seed.token,
            json!({"id": id}),
        )
        .await;
        common::assert_answer(
            &by_body,
            StatusCode::NOT_FOUND,
            &format!("the body dismissal of `{id}`"),
        );
        common::assert_error(
            &by_body,
            NOT_FOUND,
            &format!("the body dismissal of `{id}`"),
        );
    }

    let malformed_path = common::TestRequest::post(&format!("{V1}/not-an-id/dismiss"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(
        &malformed_path,
        StatusCode::NOT_FOUND,
        "a malformed path id",
    );
    common::assert_error(&malformed_path, NOT_FOUND, "a malformed path id");
    let malformed_body = post_json(
        &app,
        &format!("{V1}/dismiss"),
        &seed.token,
        json!({"id": "not-an-id"}),
    )
    .await;
    common::assert_answer(
        &malformed_body,
        StatusCode::NOT_FOUND,
        "a malformed body id",
    );
    common::assert_error(&malformed_body, NOT_FOUND, "a malformed body id");
    assert_eq!(
        listed_keys(&app, &seed.token).await.len(),
        2,
        "none of the refusals changed alice's listing"
    );
}

/// The deprecated body dismissal (`POST /api/v1/notifications/dismiss`
/// with `id`) removes the named notification and answers `{}`; a body
/// without `id` is the 400 the ticket words.
#[tokio::test]
async fn the_legacy_dismissal_works_by_body_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool);

    let response = post_json(
        &app,
        &format!("{V1}/dismiss"),
        &seed.token,
        json!({"id": seed.follow.id}),
    )
    .await;
    assert_empty_object(&response, "the legacy dismissal");
    assert_eq!(
        listed_keys(&app, &seed.token).await,
        vec![format!("favourite-{}", seed.post.id)],
        "the named row left the listing, the favourite kept its stored key"
    );

    for body in [json!({}), json!({"id": null}), json!({"other": "x"})] {
        let missing = post_json(&app, &format!("{V1}/dismiss"), &seed.token, body.clone()).await;
        common::assert_answer(
            &missing,
            StatusCode::BAD_REQUEST,
            &format!("`{body}` carries no id"),
        );
        common::assert_error(
            &missing,
            "Missing parameter: id",
            &format!("`{body}` carries no id"),
        );
    }
    let bare = common::TestRequest::post(&format!("{V1}/dismiss"))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(&bare, StatusCode::BAD_REQUEST, "an empty body");
    common::assert_error(&bare, "Missing parameter: id", "an empty body");
}

/// The form encoding of the legacy dismissal arrives the same way — the
/// extractor structures `id=…` bodies like JSON (decision 10).
#[tokio::test]
async fn the_legacy_dismissal_accepts_form_bodies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_pair(&pool).await;
    let app = common::test_app(pool);

    let response = common::TestRequest::post(&format!("{V1}/dismiss"))
        .bearer(&seed.token)
        .form(&[("id", &seed.favourite.id.to_string())])
        .send(&app)
        .await;
    assert_empty_object(&response, "the form dismissal");
    assert_eq!(
        listed_keys(&app, &seed.token).await,
        vec![ungrouped(&seed.follow)],
        "and it dismissed"
    );
}

/// `destroy_multiple` dismisses exactly the listed ids that belong to the
/// caller — from the query string, and the same from a JSON body — and
/// answers `{}` whatever else the list names.
#[tokio::test]
async fn destroy_multiple_dismisses_the_owned_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_WRITE).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let first = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let second = common::create_notification(&pool, &alice, &carol, "mention", None).await;
    let kept = common::create_notification(&pool, &alice, &carol, "move", None).await;
    let foreign = common::create_notification(&pool, &dave, &carol, "follow", None).await;
    let app = common::test_app(pool.clone());

    let route = format!(
        "{V1}/destroy_multiple?ids[]={}&ids[]={}&ids[]=not-an-id",
        first.id, foreign.id
    );
    let response = common::TestRequest::delete(&route)
        .bearer(&token)
        .send(&app)
        .await;
    assert_empty_object(&response, "the query-string bulk dismissal");
    assert_eq!(
        listed_keys(&app, &token).await,
        vec![ungrouped(&kept), ungrouped(&second)],
        "the listed row left the listing, newest first, the unlisted stayed"
    );
    let untouched = stored_row(&pool, foreign.id)
        .await
        .expect("the foreign row stays");
    assert!(
        untouched.dismissed_at.is_none(),
        "and dave's row was not touched"
    );

    let by_body = common::TestRequest::delete(&format!("{V1}/destroy_multiple"))
        .bearer(&token)
        .json(&json!({"ids": [second.id, Uuid::now_v7(), second.id]}))
        .send(&app)
        .await;
    assert_empty_object(&by_body, "the json-body bulk dismissal");
    assert_eq!(
        listed_keys(&app, &token).await,
        vec![ungrouped(&kept)],
        "the second row followed it out of the listing"
    );

    let nothing = common::TestRequest::delete(&format!("{V1}/destroy_multiple"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_empty_object(&nothing, "a bulk dismissal that names no id");
}
