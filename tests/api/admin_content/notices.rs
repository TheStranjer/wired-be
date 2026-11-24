//! The admin announcement routes over HTTP: create, serve, patch, delete and
//! the windowed listings, with the user-facing list following the writes.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::util::format::text_to_html;

use super::support::{
    ALL_ADMIN, FUTURE, NOTICE_KEYS, NOTICES, PAST, USER_NOTICES, a_notice, admin, app,
    create_notice, destroy, get, log_entry, patch,
};
use crate::common;

/// The notice create answers the entity the client API already serves —
/// fourteen keys, `all_day` and `pleroma.raw_content` included, `read`
/// false for an administration list — with the author's text rendered by
/// `text_to_html` for the wire and kept verbatim as the source; `show` and
/// the listing answer the same entity.
#[tokio::test]
async fn a_created_notice_answers_the_entity_with_its_raw_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let typed = "Down at five: https://example.org/status";
    let created = create_notice(
        &app,
        &token,
        &json!({"content": typed, "starts_at": PAST, "ends_at": FUTURE, "all_day": true}),
    )
    .await;
    assert_eq!(
        common::sorted_keys(&created),
        NOTICE_KEYS,
        "exactly the entity keys"
    );
    assert_eq!(
        created["content"],
        text_to_html(typed),
        "the served content is the plain-text render"
    );
    let rendered = created["content"].as_str().expect("html");
    assert!(
        rendered.contains("<a href=") && rendered.contains("example.org/status"),
        "with the bare URL linkified: {rendered}"
    );
    assert_eq!(
        created["pleroma"]["raw_content"], typed,
        "while the source is kept verbatim"
    );
    assert_eq!(
        common::sorted_keys(&created["pleroma"]),
        ["raw_content"],
        "the pleroma block carries that key alone"
    );
    assert_eq!(created["starts_at"], PAST, "the window, mastodon times");
    assert_eq!(created["ends_at"], FUTURE, "at both ends");
    assert_eq!(created["all_day"], true, "the stored date-only flag");
    assert_eq!(
        created["read"], false,
        "an admin list computes no dismissal"
    );
    assert_eq!(
        created["published_at"], created["updated_at"],
        "a fresh row is stamped once"
    );

    let shown = get(
        &app,
        &token,
        &format!("{NOTICES}/{}", common::id_of(&created)),
    )
    .await;
    common::assert_answer(&shown, StatusCode::OK, "reading one notice");
    assert_eq!(
        shown.json_value(),
        created,
        "show answers the created entity"
    );

    let listed = get(&app, &token, NOTICES).await;
    common::assert_answer(&listed, StatusCode::OK, "listing the notices");
    assert_eq!(
        listed.json_value(),
        json!([created]),
        "the listing is a bare array of the same entities"
    );
}

/// A plain-text notice is the render its author's words get: markup typed
/// into the body comes back escaped in `content`, verbatim in
/// `pleroma.raw_content` — and inert on the user's own read.
#[tokio::test]
async fn a_notice_serves_the_render_of_what_its_author_typed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;

    let typed = "New <script>alert(1)</script> text";
    let entity = a_notice(&app, &token, typed).await;
    assert_eq!(
        entity["content"], "<p>New &lt;script&gt;alert(1)&lt;/script&gt; text</p>",
        "the markup the author typed is inert text in the served content"
    );
    assert_eq!(
        entity["pleroma"]["raw_content"], typed,
        "and the source keeps every byte of it"
    );

    let mine = get(&app, &read, USER_NOTICES).await.json_value();
    assert_eq!(
        mine[0], entity,
        "the user's read is the same entity, unread"
    );
}

/// `PATCH /announcements/{id}` is present-only over the four parameters: a
/// text write replaces both spellings and leaves the window and flag alone,
/// a window write leaves the text alone, and the row reads back as written.
#[tokio::test]
async fn a_notice_patch_changes_only_the_fields_it_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let stored = create_notice(
        &app,
        &token,
        &json!({"content": "before", "starts_at": PAST, "ends_at": FUTURE, "all_day": true}),
    )
    .await;
    let id = common::id_of(&stored);

    let typed = "New <script>alert(1)</script> text";
    let rendered = "<p>New &lt;script&gt;alert(1)&lt;/script&gt; text</p>";
    let retitled = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"content": typed}),
    )
    .await;
    common::assert_answer(&retitled, StatusCode::OK, "replacing the text");
    let body = retitled.json_value();
    assert_eq!(body["content"], rendered, "the new text re-rendered");
    assert_eq!(
        body["pleroma"]["raw_content"], typed,
        "with the source kept verbatim"
    );
    assert_eq!(
        body["starts_at"], PAST,
        "the window the write did not name stands"
    );
    assert_eq!(body["ends_at"], FUTURE, "at both ends");
    assert_eq!(body["all_day"], true, "and so does the flag");

    let closed = patch(
        &app,
        &token,
        &format!("{NOTICES}/{id}"),
        &json!({"all_day": false, "ends_at": "2000-06-02T00:00:00Z"}),
    )
    .await;
    common::assert_answer(&closed, StatusCode::OK, "closing the window");
    let window = closed.json_value();
    assert_eq!(window["all_day"], false, "the flag flipped");
    assert_eq!(
        window["ends_at"], "2000-06-02T00:00:00.000Z",
        "the bound replaced"
    );
    assert_eq!(
        window["content"], rendered,
        "the stored render survives a write that did not name it"
    );
    assert_eq!(
        get(&app, &token, &format!("{NOTICES}/{id}"))
            .await
            .json_value(),
        window,
        "and it reads back as written"
    );

    let (message, data) = log_entry(&pool, "announcement_update").await;
    assert_eq!(
        message,
        format!("@root updated announcement: {id}"),
        "the write is logged"
    );
    assert_eq!(data["subject_id"], id, "by the notice's own id");
}

/// `DELETE /announcements/{id}` answers `{}` and takes the notice out of
/// the admin and the user reads alike, its create and delete entries the
/// surviving trace.
#[tokio::test]
async fn a_deleted_notice_answers_an_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let keeper = a_notice(&app, &token, "keeper").await;
    let doomed = a_notice(&app, &token, "doomed").await;
    let id = common::id_of(&doomed);

    let gone = destroy(&app, &token, &format!("{NOTICES}/{id}")).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting a notice");
    assert_eq!(
        gone.json_value(),
        json!({}),
        "the delete answers the empty object"
    );

    let missing = get(&app, &token, &format!("{NOTICES}/{id}")).await;
    common::assert_answer(
        &missing,
        StatusCode::NOT_FOUND,
        "the deleted notice is gone",
    );
    common::assert_error(&missing, "Not found", "the deleted notice is gone");
    assert_eq!(
        common::ids_of(&get(&app, &read, USER_NOTICES).await.json_value()),
        vec![common::id_of(&keeper)],
        "the user's list keeps only what was not deleted"
    );

    for (action, verb) in [
        ("announcement_create", "created"),
        ("announcement_delete", "deleted"),
    ] {
        let (message, data) = log_entry(&pool, action).await;
        assert_eq!(
            message,
            format!("@root {verb} announcement: {id}"),
            "the {action} sentence names the notice"
        );
        assert_eq!(data["subject_id"], id, "and carries its id");
    }
}

/// The listing window: `limit` and `offset` cut the newest-first page, an
/// absent or unusable parameter falls back to the admin default, and an
/// `offset` past the last row answers nothing.
#[tokio::test]
async fn the_announcement_listing_windows_by_limit_and_offset() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let mut made = Vec::new();
    for number in 0..4 {
        let entity = a_notice(&app, &token, &format!("notice {number}")).await;
        made.push(common::id_of(&entity));
    }
    let newest_first: Vec<String> = made.iter().rev().cloned().collect();

    let head = get(&app, &token, &format!("{NOTICES}?limit=2&offset=1")).await;
    common::assert_answer(&head, StatusCode::OK, "a windowed listing");
    assert_eq!(
        common::ids_of(&head.json_value()),
        vec![newest_first[1].clone(), newest_first[2].clone()],
        "the second and third rows of the newest-first list"
    );

    let whole = get(&app, &token, NOTICES).await.json_value();
    assert_eq!(
        common::ids_of(&whole),
        newest_first,
        "no parameters: the whole list"
    );
    let junk = get(&app, &token, &format!("{NOTICES}?limit=no&offset=-3")).await;
    assert_eq!(
        common::ids_of(&junk.json_value()),
        newest_first,
        "an unusable pair falls back to the default window"
    );
    assert_eq!(
        get(&app, &token, &format!("{NOTICES}?offset=90"))
            .await
            .json_value(),
        json!([]),
        "an offset past the last row answers nothing"
    );
    assert_eq!(
        common::ids_of(
            &get(&app, &token, &format!("{NOTICES}?limit=100000"))
                .await
                .json_value()
        ),
        newest_first,
        "the admin ceiling on `limit` changes nothing for four rows"
    );
}

/// The administration view sees every notice — the one whose activity has
/// not opened, the one whose activity closed and the live one — while the
/// user-facing list serves only the live one, as the same entity.
#[tokio::test]
async fn the_admin_sees_notices_whose_window_excludes_the_present() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;

    let upcoming = create_notice(
        &app,
        &token,
        &json!({"content": "later", "starts_at": FUTURE}),
    )
    .await;
    let expired = create_notice(&app, &token, &json!({"content": "over", "ends_at": PAST})).await;
    let live = a_notice(&app, &token, "now").await;

    let listed = get(&app, &token, NOTICES).await.json_value();
    assert_eq!(
        common::ids_of(&listed),
        vec![
            common::id_of(&live),
            common::id_of(&expired),
            common::id_of(&upcoming)
        ],
        "all three, newest first, window aside"
    );

    let mine = get(&app, &read, USER_NOTICES).await;
    common::assert_answer(&mine, StatusCode::OK, "the user's list");
    assert_eq!(
        mine.json_value(),
        json!([live]),
        "the user sees only what is active, as the same entity"
    );
}

/// A user-facing list is what an admin write changes: opening an upcoming
/// notice puts it in the user's window, and deleting the live one takes it
/// out again.
#[tokio::test]
async fn a_user_facing_list_follows_the_admin_writes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let read = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let upcoming = create_notice(
        &app,
        &token,
        &json!({"content": "later", "starts_at": FUTURE}),
    )
    .await;
    let live = a_notice(&app, &token, "now").await;

    let opened = patch(
        &app,
        &token,
        &format!("{NOTICES}/{}", common::id_of(&upcoming)),
        &json!({"starts_at": PAST}),
    )
    .await;
    common::assert_answer(&opened, StatusCode::OK, "opening the upcoming notice");
    let after = get(&app, &read, USER_NOTICES).await.json_value();
    assert_eq!(
        common::ids_of(&after),
        vec![common::id_of(&live), common::id_of(&upcoming)],
        "both active now, newest first"
    );
    assert_eq!(
        after[1]["content"], "<p>later</p>",
        "with the text the admin sent"
    );

    let gone = destroy(&app, &token, &format!("{NOTICES}/{}", common::id_of(&live))).await;
    common::assert_answer(&gone, StatusCode::OK, "deleting the live notice");
    assert_eq!(
        common::ids_of(&get(&app, &read, USER_NOTICES).await.json_value()),
        vec![common::id_of(&upcoming)],
        "and it leaves the user's list with it"
    );
}
