//! The client-to-server outbox (`POST /users/{nickname}/outbox`): its
//! owner-and-scope gates, the `Create` of a `Note` posting end to end with
//! its warning, fold, audience and claimed upload, the reply and
//! unsupported-shape arms, and the `Delete` of the caller's own status.

use axum::http::StatusCode;
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::models::{
    activitypub_actor::ActivityPubActorWrite, status::Visibility, user::followers,
};

use super::support::{
    AS_PUBLIC, BASE, c2s_account, create_note, location_status_id, png_fixture, post_outbox,
    show_status, upload_to_ap,
};
use crate::common;

/// The C2S outbox gates: no token 401, another account's token the 403
/// `{"error":"Forbidden"}`, and a token without `write:statuses` the scope
/// refusal — before a single byte of the activity is looked at.
#[tokio::test]
async fn the_c2s_outbox_only_answers_its_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, read_only) = c2s_account(&pool, "c2s-out-gate-a", "read").await;
    let (_bob, bob_token) = c2s_account(&pool, "c2s-out-gate-b", "read write").await;

    let anonymous = post_outbox(
        &app,
        "c2s-out-gate-a",
        None,
        create_note(&json!([AS_PUBLIC]), &json!([]), "hi"),
    )
    .await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "outbox, no token");

    let stranger = post_outbox(
        &app,
        "c2s-out-gate-a",
        Some(&bob_token),
        create_note(&json!([AS_PUBLIC]), &json!([]), "hi"),
    )
    .await;
    common::assert_answer(&stranger, StatusCode::FORBIDDEN, "outbox of another");
    common::assert_error(&stranger, "Forbidden", "outbox of another");

    let unscoped = post_outbox(
        &app,
        "c2s-out-gate-a",
        Some(&read_only),
        create_note(&json!([AS_PUBLIC]), &json!([]), "hi"),
    )
    .await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "outbox, no write scope");
    common::assert_error(
        &unscoped,
        "Insufficient permissions: write:statuses.",
        "outbox, no write scope",
    );
}

/// `Create` of a `Note` through the outbox posts end to end: 201 with the
/// `Create` activity's id in `Location` and an empty body, and the status
/// readable at `/api/v1/statuses/{id}` with the note's text, warning and
/// fold, and the upload the note attached.
#[tokio::test]
async fn the_c2s_outbox_create_posts_end_to_end() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_alice, token) = c2s_account(&pool, "c2s-out-create", "read write").await;

    let upload = upload_to_ap(&app, Some(&token), png_fixture(4), Some("a gradient")).await;
    assert_eq!(upload.status, StatusCode::OK, "the upload lands");
    let media_id = upload.json_value()["id"]
        .as_str()
        .expect("the media id")
        .to_owned();

    let activity = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "hello from c2s",
            "summary": "a warning",
            "sensitive": true,
            "attachment": [{"type": "Document", "id": media_id}],
        },
    });
    let created = post_outbox(&app, "c2s-out-create", Some(&token), activity).await;
    assert_eq!(created.status, StatusCode::CREATED, "the create fires");
    assert!(created.body.is_empty(), "and answers no body");
    let location = created.header("location").expect("a Location").to_owned();

    let shown = show_status(&app, &token, location_status_id(&created)).await;
    assert_eq!(
        shown.status,
        StatusCode::OK,
        "the post is visible to its author"
    );
    let entity = shown.json_value();
    assert_eq!(
        entity["uri"],
        format!("{BASE}/objects/{}", entity["id"].as_str().expect("the id")),
        "the stored object id under the test instance"
    );
    assert_eq!(
        entity["content"], "<p>hello from c2s</p>",
        "the text rode through"
    );
    assert_eq!(
        entity["spoiler_text"], "a warning",
        "and the summary became the warning"
    );
    assert_eq!(entity["sensitive"], true, "and the fold its flag");
    assert_eq!(
        entity["visibility"], "public",
        "to the public, as `to` said"
    );
    assert_eq!(
        entity["media_attachments"][0]["id"], media_id,
        "the attachment id claimed the upload"
    );
    assert_eq!(
        location,
        format!(
            "{BASE}/activities/{}",
            entity["id"].as_str().expect("the id")
        ),
        "the Location is the Create activity the post federates as"
    );
}

/// The create's visibility follows the note's and activity's `to`/`cc`
/// exactly as the federation parser reads them: `Public` in `to` is
/// public, in `cc` alone unlisted, the followers collection private,
/// anything else direct.
#[tokio::test]
async fn the_c2s_outbox_create_reads_visibility_from_the_audience() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "c2s-out-visibility").await;
    let _row = common::write_actor(
        &pool,
        &ActivityPubActorWrite {
            followers_url: Some(followers(&alice.ap_id)),
            ..ActivityPubActorWrite::bare(alice.id)
        },
    )
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    let followers = followers(&alice.ap_id);

    let cases = [
        (json!([AS_PUBLIC]), json!([]), "public"),
        (json!([]), json!([AS_PUBLIC]), "unlisted"),
        (json!([followers.clone()]), json!([]), "private"),
        (json!([]), json!([]), "direct"),
    ];
    for (index, (to, cc, want)) in cases.iter().enumerate() {
        let activity = create_note(to, cc, &format!("audience case {index}"));
        let created = post_outbox(&app, "c2s-out-visibility", Some(&token), activity).await;
        assert_eq!(created.status, StatusCode::CREATED, "case {index} posts");
        let shown = show_status(&app, &token, location_status_id(&created)).await;
        assert_eq!(
            shown.json_value()["visibility"],
            json!(*want),
            "case {index} stores the audience its addressing spells"
        );
    }
}

/// The create's reply and object arms: an `inReplyTo` naming a stored
/// status lands the answer on it, one naming nothing is the 404, and
/// every shape the outbox does not fold — a non-`Note` object, a bare
/// reference, an unknown type, an empty body — is the 400
/// `{"error":"Unsupported activity type"}`.
#[tokio::test]
async fn the_c2s_outbox_create_replies_and_refuses_other_shapes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-out-shape", "read write").await;
    let parent = common::create_status(&pool, &alice, "a parent", Visibility::Public).await;

    let reply = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "an answer",
            "inReplyTo": parent.uri,
        },
    });
    let created = post_outbox(&app, "c2s-out-shape", Some(&token), reply).await;
    assert_eq!(created.status, StatusCode::CREATED, "the answer posts");
    let shown = show_status(&app, &token, location_status_id(&created)).await;
    assert_eq!(
        shown.json_value()["in_reply_to_id"],
        parent.id.to_string(),
        "and lands on the parent it named"
    );

    let stranger = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": { "type": "Note", "content": "x", "inReplyTo": "https://nowhere.example/objects/1" },
    });
    let refused = post_outbox(&app, "c2s-out-shape", Some(&token), stranger).await;
    common::assert_answer(
        &refused,
        StatusCode::NOT_FOUND,
        "a parent that is no stored status",
    );

    for unsupported in [
        json!({"type": "Create", "object": {"type": "Question", "content": "poll?"}}),
        json!({"type": "Create", "object": format!("{BASE}/objects/whatever")}),
        json!({"type": "Flag", "object": "x"}),
        json!({"type": "Listen"}),
        json!({}),
    ] {
        let denied = post_outbox(&app, "c2s-out-shape", Some(&token), unsupported.clone()).await;
        common::assert_answer(
            &denied,
            StatusCode::BAD_REQUEST,
            &format!("unsupported shape: {unsupported}"),
        );
        common::assert_error(
            &denied,
            "Unsupported activity type",
            "an unfoldable activity",
        );
    }
}

/// `Delete` of the caller's own status by object id: the post goes out of
/// `GET /api/v1/statuses/{id}` with a 201 naming the `Delete`; somebody
/// else's post is the 403 and an unknown reference the 404.
#[tokio::test]
async fn the_c2s_outbox_delete_takes_only_its_own_posts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = c2s_account(&pool, "c2s-out-delete-a", "read write").await;
    let (bob, bob_token) = c2s_account(&pool, "c2s-out-delete-b", "read write").await;

    let activity = create_note(&json!([AS_PUBLIC]), &json!([]), "doomed");
    let created = post_outbox(&app, "c2s-out-delete-a", Some(&alice_token), activity).await;
    assert_eq!(created.status, StatusCode::CREATED, "the post fires");
    let id = location_status_id(&created);
    let uri = format!("{BASE}/objects/{id}");

    let deleted = post_outbox(
        &app,
        "c2s-out-delete-a",
        Some(&alice_token),
        json!({"type": "Delete", "object": uri}),
    )
    .await;
    assert_eq!(
        deleted.status,
        StatusCode::CREATED,
        "the delete is accepted"
    );
    assert!(deleted.body.is_empty(), "and answers no body");
    assert!(
        deleted
            .header("location")
            .expect("a Location")
            .starts_with(&format!("{BASE}/activities/")),
        "the Location names the Delete activity"
    );
    let shown = show_status(&app, &alice_token, id).await;
    common::assert_answer(&shown, StatusCode::NOT_FOUND, "the post after its delete");

    let foreign = common::create_status(&pool, &bob, "bobs own", Visibility::Public).await;
    let refused = post_outbox(
        &app,
        "c2s-out-delete-a",
        Some(&alice_token),
        json!({"type": "Delete", "object": foreign.uri}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "a delete of another's post",
    );
    common::assert_error(&refused, "Forbidden", "a delete of another's post");

    let unknown = post_outbox(
        &app,
        "c2s-out-delete-a",
        Some(&alice_token),
        json!({"type": "Delete", "object": format!("{BASE}/objects/{}", Uuid::now_v7())}),
    )
    .await;
    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "a delete of nothing");

    // Bob may of course delete his own — by id text as well as by uri.
    let by_id = post_outbox(
        &app,
        "c2s-out-delete-b",
        Some(&bob_token),
        json!({"type": "Delete", "object": foreign.id.to_string()}),
    )
    .await;
    assert_eq!(
        by_id.status,
        StatusCode::CREATED,
        "the id text names it too"
    );
}
