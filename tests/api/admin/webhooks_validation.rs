//! The webhook API's refusals and actions: the pinned 404 on every
//! `{id}` route, the create's body validation and the patch's screen with
//! their `{"errors": …}` shapes writing nothing, and the `disable`,
//! `enable` and `rotate_secret` actions each turning exactly what it
//! names.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::support::NOT_FOUND;
use super::webhooks_crud::{
    WEBHOOKS, act_on_webhook, assert_secret, create_webhook, post_webhook, stored_webhook,
    webhook_admin,
};
use crate::common::{self, TestRequest};

/// Every `{id}` route answers the same missing row alike: an unknown uuid
/// and text that is no id both get the 404 `{"error":"Not found"}`, on
/// the show, the patch, the delete and all three action routes.
#[tokio::test]
async fn unknown_ids_are_the_pinned_404_on_every_id_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookmisser").await;

    for id in [Uuid::now_v7().to_string(), "not-a-uuid".to_owned()] {
        let responses = [
            TestRequest::get(&format!("{WEBHOOKS}/{id}"))
                .bearer(&token)
                .send(&app)
                .await,
            TestRequest::patch(&format!("{WEBHOOKS}/{id}"))
                .bearer(&token)
                .json(&json!({"enabled": false}))
                .send(&app)
                .await,
            TestRequest::delete(&format!("{WEBHOOKS}/{id}"))
                .bearer(&token)
                .send(&app)
                .await,
            post_webhook(&app, &token, &format!("{WEBHOOKS}/{id}/enable"), json!({})).await,
            post_webhook(&app, &token, &format!("{WEBHOOKS}/{id}/disable"), json!({})).await,
            post_webhook(
                &app,
                &token,
                &format!("{WEBHOOKS}/{id}/rotate_secret"),
                json!({}),
            )
            .await,
        ];
        common::assert_all_refused(
            &responses,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("the routes on /{id}"),
        );
    }
}

/// The create's body validation, over HTTP: a missing `url`, an
/// unroutable `url` and an unknown event each answer the pinned 400
/// `{"errors": …}` shape, an unreadable `enabled` is the extractor's 422,
/// and no refused body writes a row.
#[tokio::test]
async fn webhook_creates_are_validated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookvalidator").await;

    let bodiless = post_webhook(
        &app,
        &token,
        WEBHOOKS,
        json!({"events": ["report.created"]}),
    )
    .await;
    common::assert_answer(&bodiless, StatusCode::BAD_REQUEST, "a create without a url");
    assert_eq!(
        bodiless.json_value(),
        json!({"errors": {"url": ["Missing parameter: url"]}}),
        "the missing-parameter shape"
    );

    let unroutable = post_webhook(
        &app,
        &token,
        WEBHOOKS,
        json!({"url": "ftp://example.org/hook"}),
    )
    .await;
    common::assert_answer(&unroutable, StatusCode::BAD_REQUEST, "an ftp url");
    assert_eq!(
        unroutable.json_value(),
        json!({"errors": {"url": ["invalid url"]}}),
        "no web scheme is no delivery target"
    );

    let unknown_event = post_webhook(
        &app,
        &token,
        WEBHOOKS,
        json!({"url": "https://example.org/hook", "events": ["report.deleted"]}),
    )
    .await;
    common::assert_answer(&unknown_event, StatusCode::BAD_REQUEST, "an unknown event");
    assert_eq!(
        unknown_event.json_value(),
        json!({"errors": {"events": ["invalid event"]}}),
        "the wording the ticket pins"
    );

    let bad_gate = post_webhook(
        &app,
        &token,
        WEBHOOKS,
        json!({"url": "https://example.org/hook", "enabled": "maybe"}),
    )
    .await;
    common::assert_answer(
        &bad_gate,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unreadable gate",
    );

    let listed = TestRequest::get(WEBHOOKS).bearer(&token).send(&app).await;
    assert_eq!(
        listed.json_value(),
        json!([]),
        "a refused body wrote nothing"
    );
}

/// The `PATCH` screens its present fields exactly as the create does —
/// the same 400 `{"errors": …}` bodies for an unknown event and a bad
/// url — and leaves the row untouched on refusal.
#[tokio::test]
async fn webhook_patches_are_screened_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookscreener").await;

    let created = post_webhook(
        &app,
        &token,
        WEBHOOKS,
        json!({"url": "https://example.org/hook", "events": ["report.created"]}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the control create");
    let entity = created.json_value();
    let id = entity["id"].as_str().expect("the id is text").to_owned();

    let bad_events = TestRequest::patch(&format!("{WEBHOOKS}/{id}"))
        .bearer(&token)
        .json(&json!({"events": ["nonsense"]}))
        .send(&app)
        .await;
    common::assert_answer(
        &bad_events,
        StatusCode::BAD_REQUEST,
        "a patch with an unknown event",
    );
    assert_eq!(
        bad_events.json_value(),
        json!({"errors": {"events": ["invalid event"]}}),
        "the same refusal on the patch"
    );
    let bad_url = TestRequest::patch(&format!("{WEBHOOKS}/{id}"))
        .bearer(&token)
        .json(&json!({"url": "mailto:hook@example.org"}))
        .send(&app)
        .await;
    assert_eq!(
        bad_url.json_value(),
        json!({"errors": {"url": ["invalid url"]}}),
        "and the same url screen"
    );

    let shown = TestRequest::get(&format!("{WEBHOOKS}/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        shown.json_value(),
        entity,
        "the refused patches changed nothing"
    );
}

/// The action routes: `disable` and `enable` answer the entity with the
/// gate set and leave the rest of the row standing; `rotate_secret` turns
/// only the secret, in the minted shape, every time.
#[tokio::test]
async fn webhooks_enable_disable_and_rotate_their_secret() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookgater").await;

    let (entity, id_text) = create_webhook(
        &app,
        &token,
        json!({"url": "https://example.org/hook", "events": ["report.created"]}),
    )
    .await;
    let id = Uuid::parse_str(&id_text).expect("the test id parses");

    let disabled = act_on_webhook(&app, &token, &id_text, "/disable").await;
    common::assert_answer(&disabled, StatusCode::OK, "the disable");
    let off = disabled.json_value();
    assert_eq!(off["enabled"], json!(false), "the gate closed");
    assert_eq!(
        off["secret"], entity["secret"],
        "the disable leaves the secret"
    );
    let off_row = stored_webhook(&pool, id).await.expect("the row stands");
    assert!(!off_row.3, "the table says disabled");

    let enabled = act_on_webhook(&app, &token, &id_text, "/enable").await;
    common::assert_answer(&enabled, StatusCode::OK, "the enable");
    let on = enabled.json_value();
    assert_eq!(on["enabled"], json!(true), "the gate opened again");
    assert_eq!(
        on["url"], entity["url"],
        "and the rest of the entity stands"
    );

    let rotated = act_on_webhook(&app, &token, &id_text, "/rotate_secret").await;
    common::assert_answer(&rotated, StatusCode::OK, "the rotation");
    let after = rotated.json_value();
    assert_ne!(
        after["secret"], entity["secret"],
        "rotate_secret answers a NEW secret"
    );
    assert_secret(&after);
    assert_eq!(after["id"], entity["id"], "everything else stands");
    assert_eq!(after["url"], entity["url"], "id, url");
    assert_eq!(after["events"], entity["events"], "events");
    assert_eq!(after["enabled"], entity["enabled"], "gate");
    assert_eq!(after["internal"], entity["internal"], "flag");
    let rotated_row = stored_webhook(&pool, id).await.expect("the row stands");
    assert_eq!(
        rotated_row.2,
        after["secret"].as_str().expect("the secret is text"),
        "the row carries the new secret"
    );

    let twice = act_on_webhook(&app, &token, &id_text, "/rotate_secret").await;
    assert_ne!(
        twice.json_value()["secret"],
        after["secret"],
        "and a second rotation mints again"
    );
}
