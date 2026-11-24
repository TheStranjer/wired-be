//! Announcement refusals, the 404s, the stream event a create publishes (and
//! the patch and delete that publish none) and the form-body writes.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;
use wired_be::streaming::hub::Audience;

use super::support::{
    ALL_ADMIN, BLANK, ENDS_AFTER, FUTURE, INVALID_TIME, NOTICES, PAST, RULES, a_notice, admin, app,
    app_with_state, create_notice, destroy, get, notice_count, patch, post_notice,
};
use crate::common;

/// A notice body's refusals, each leaving no row: `content` is required, a
/// bound that is no RFC 3339 timestamp is refused by name (both together
/// when both are unusable), and a window whose end does not fall strictly
/// after its start is refused on `ends_at`.
#[tokio::test]
async fn a_notice_body_refuses_blank_content_and_a_bad_window() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    for body in [
        json!({}),
        json!({"content": ""}),
        json!({"content": "  \n "}),
    ] {
        let refused = post_notice(&app, &token, &body).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a blank content");
        assert_eq!(
            refused.json_value(),
            json!({"errors": {"content": [BLANK]}}),
            "the required-text refusal: {}",
            refused.text()
        );
    }

    let refusals = [
        (
            json!({"content": "x", "starts_at": FUTURE, "ends_at": PAST}),
            json!({"errors": {"ends_at": [ENDS_AFTER]}}),
        ),
        (
            json!({"content": "x", "starts_at": PAST, "ends_at": PAST}),
            json!({"errors": {"ends_at": [ENDS_AFTER]}}),
        ),
        (
            json!({"content": "x", "starts_at": "yesterday", "ends_at": "06/01/2100"}),
            json!({"errors": {"starts_at": [INVALID_TIME], "ends_at": [INVALID_TIME]}}),
        ),
        (
            json!({"content": "x", "starts_at": "2026-02-01"}),
            json!({"errors": {"starts_at": [INVALID_TIME]}}),
        ),
    ];
    for (body, expected) in refusals {
        let refused = post_notice(&app, &token, &body).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a refused window");
        assert_eq!(
            refused.json_value(),
            expected,
            "the refusal the body earns: {}",
            refused.text()
        );
    }
    assert_eq!(notice_count(&pool).await, 0, "and no refusal wrote a row");
}

/// A `PATCH` checks the window it merges, not only the pair it was sent: a
/// `null` bound reads as absent, so the stored one still decides, and an
/// end moved before it is the same refusal with the row left alone.
#[tokio::test]
async fn a_notice_patch_checks_the_window_it_merges() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let stored = create_notice(
        &app,
        &token,
        &json!({"content": "anchored", "starts_at": PAST}),
    )
    .await;
    let id = common::id_of(&stored);

    let nulled = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"starts_at": null, "ends_at": "2000-06-02T00:00:00Z"}),
    )
    .await;
    common::assert_answer(&nulled, StatusCode::OK, "a null bound is no bound to write");
    let body = nulled.json_value();
    assert_eq!(body["starts_at"], PAST, "the stored start stands");
    assert_eq!(
        body["ends_at"], "2000-06-02T00:00:00.000Z",
        "and the new end is the one written"
    );

    let crossed = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"ends_at": "1999-01-01T00:00:00Z"}),
    )
    .await;
    common::assert_answer(
        &crossed,
        StatusCode::BAD_REQUEST,
        "the merged window is checked",
    );
    assert_eq!(
        crossed.json_value(),
        json!({"errors": {"ends_at": [ENDS_AFTER]}}),
        "against the stored start, not just the sent pair"
    );
    let unchanged = get(&app, &token, &format!("{NOTICES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        unchanged["ends_at"], "2000-06-02T00:00:00.000Z",
        "the refusal wrote nothing"
    );
}

/// A notice id that is no uuid and one that names no row are the 404
/// `{"error":"Not found"}` on all three routes that take one.
#[tokio::test]
async fn an_unknown_announcement_is_the_not_found_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    for path in [
        format!("{NOTICES}/nope"),
        format!("{NOTICES}/{}", Uuid::now_v7()),
    ] {
        let shown = get(&app, &token, &path).await;
        common::assert_answer(&shown, StatusCode::NOT_FOUND, &format!("showing {path}"));
        common::assert_error(&shown, "Not found", &format!("showing {path}"));

        let changed = patch(&app, &token, &path, &json!({"content": "ghost"})).await;
        common::assert_answer(&changed, StatusCode::NOT_FOUND, "updating no notice");
        common::assert_error(&changed, "Not found", "updating no notice");

        let gone = destroy(&app, &token, &path).await;
        common::assert_answer(&gone, StatusCode::NOT_FOUND, "deleting no notice");
        common::assert_error(&gone, "Not found", "deleting no notice");
    }
    assert_eq!(
        notice_count(&pool).await,
        0,
        "and nothing was written on the way"
    );
}

/// A created notice is published to the sockets as Mastodon's
/// `announcement` event — the entity as the string-in-envelope, addressed
/// to every active local account.
#[tokio::test]
async fn a_created_notice_publishes_the_announcement_event() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = app_with_state(&pool);
    let (root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let mut rx = state.hub.subscribe();

    let refused = post_notice(&app, &token, &json!({"content": "  "})).await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "a blank create");
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "a refused write publishes nothing"
    );

    let entity = a_notice(&app, &token, "We are up").await;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the create published an event")
        .expect("the hub holds it");
    assert_eq!(event.event, "announcement", "Mastodon's announcement event");
    let mut streams: Vec<String> = event.streams.clone();
    streams.sort();
    let mut expected: Vec<String> = vec![format!("user:{}", root.id), format!("user:{}", alice.id)];
    expected.sort();
    assert_eq!(
        streams, expected,
        "one user stream per active local account"
    );
    assert!(
        matches!(&event.audience, Audience::Users(ids) if ids.len() == 2),
        "the same roster is the whole audience"
    );
    let payload: Value = serde_json::from_str(
        event
            .payload
            .as_deref()
            .expect("the event carries the entity"),
    )
    .expect("the payload is the entity's JSON");
    assert_eq!(payload, entity, "byte for byte what the route answered");
    assert!(
        event.render.is_none() && !event.local_only,
        "the entity rides, unflagged"
    );
}

/// The other writes keep the sockets quiet: Mastodon announces a new notice
/// only, so neither a patch nor a delete publishes anything.
#[tokio::test]
async fn neither_a_patched_nor_a_deleted_notice_is_published() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = app_with_state(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let entity = a_notice(&app, &token, "once").await;
    let id = common::id_of(&entity);
    let mut rx = state.hub.subscribe();

    let patched = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"content": "twice"}),
    )
    .await;
    common::assert_answer(&patched, StatusCode::OK, "patching the notice");
    let gone = destroy(&app, &token, &format!("{NOTICES}/{id}")).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting the notice");
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "nothing after the create waits on the hub"
    );
}

/// A form-encoded body works as well as a JSON one: the stringly-typed
/// `priority` and `all_day` read through the same flex helpers a client's
/// form request needs, and the refusals are the same.
#[tokio::test]
async fn a_form_body_writes_both_kinds_of_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let rule_response = common::TestRequest::post(RULES)
        .bearer(&token)
        .form(&[("text", "No spam"), ("priority", "-3")])
        .send(&app)
        .await;
    common::assert_answer(&rule_response, StatusCode::OK, "a form-encoded rule");
    let body = rule_response.json_value();
    assert_eq!(
        body,
        json!({"id": body["id"], "text": "No spam", "hint": "", "priority": -3}),
        "the string number read as an integer"
    );

    let notice_response = common::TestRequest::post(NOTICES)
        .bearer(&token)
        .form(&[
            ("content", "Form text"),
            ("all_day", "true"),
            ("starts_at", PAST),
        ])
        .send(&app)
        .await;
    common::assert_answer(&notice_response, StatusCode::OK, "a form-encoded notice");
    let written = notice_response.json_value();
    assert_eq!(written["content"], "<p>Form text</p>", "the text rendered");
    assert_eq!(
        written["raw_content"],
        Value::Null,
        "which is no entity key"
    );
    assert_eq!(
        written["pleroma"]["raw_content"], "Form text",
        "the source is under the pleroma block"
    );
    assert_eq!(written["all_day"], true, "the string gate read as true");
    assert_eq!(written["starts_at"], PAST, "and the bound parsed");

    let blank = common::TestRequest::post(RULES)
        .bearer(&token)
        .form(&[("text", " ")])
        .send(&app)
        .await;
    assert_eq!(
        blank.json_value(),
        json!({"errors": {"text": [BLANK]}}),
        "the same refusal in a form body"
    );
}
