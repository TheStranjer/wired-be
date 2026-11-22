//! Replacing, filtering and clearing a post's media in an edit.

use super::*;

/// `media_ids` replaces the post's whole attachment set: the named upload
/// arrives, the previous one is freed, and the superseded version's history
/// entry still names the media the post wore before.
#[tokio::test]
async fn editing_media_replaces_the_posts_attachments() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "media-editor").await;
    let row = post(&pool, &alice, "pictures at an edit").await;
    let old = attach(&pool, &alice, &row, 0).await;
    let fresh = upload(&pool, &alice).await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"media_ids": [fresh.to_string()]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the media edit passes");
    let body = response.json_value();
    let media = body["media_attachments"]
        .as_array()
        .expect("media is an array");
    assert_eq!(media.len(), 1, "the new set is the named one");
    assert_eq!(media[0]["id"], fresh.to_string());
    assert_eq!(
        stored_media(&pool, row.id).await,
        vec![fresh],
        "and the store agrees"
    );
    let old_row = media_attachment::get_by_id(&pool, old)
        .await
        .unwrap()
        .expect("the freed attachment stays in the table");
    assert!(
        old_row.status_id.is_none(),
        "the un-named one is freed, not deleted"
    );

    let versions = history(&app, Some(&token), &row.id.to_string())
        .await
        .json_value();
    let items = versions.as_array().expect("the history is an array");
    assert_eq!(items.len(), 2, "a media change superseded a version");
    let before = items[0]["media_attachments"]
        .as_array()
        .expect("media is an array");
    assert_eq!(before.len(), 1, "the snapshot names the old attachment");
    assert_eq!(before[0]["id"], old.to_string());
}

/// Media ids that name nothing the author may attach — unknown, malformed,
/// another account's upload — drop out of the replacement, exactly as
/// Pleroma's `attachments_from_ids` rejects its nils.
#[tokio::test]
async fn media_nobody_may_attach_drops_out_of_the_replacement() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "picker").await;
    let (bob, _) = editor(&pool, "other-picker").await;
    let row = post(&pool, &alice, "mine only").await;
    let mine = upload(&pool, &alice).await;
    let theirs = upload(&pool, &bob).await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"media_ids": [mine.to_string(), UNKNOWN_ID, "not-a-uuid", theirs.to_string()]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the edit passes");
    assert_eq!(
        stored_media(&pool, row.id).await,
        vec![mine],
        "the usable id survives and the rest name nothing"
    );
}

/// An empty `media_ids` list strips the post of every attachment; an
/// absent one says nothing about media.
#[tokio::test]
async fn media_can_be_cleared_and_an_absent_list_touches_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "unposter").await;
    let row = post(&pool, &alice, "text only after all").await;
    let one = attach(&pool, &alice, &row, 0).await;

    let keep = edit_row(&app, &token, &row, json!({"spoiler_text": "cw"})).await;
    common::assert_answer(&keep, StatusCode::OK, "the non-media edit passes");
    assert_eq!(
        stored_media(&pool, row.id).await,
        vec![one],
        "a body that names no media leaves the post's set exactly as it is"
    );

    let cleared = edit_row(&app, &token, &row, json!({"media_ids": []})).await;
    common::assert_answer(&cleared, StatusCode::OK, "the clearing edit passes");
    assert_eq!(
        cleared.json_value()["media_attachments"],
        json!([]),
        "and a submitted empty list strips the post"
    );
    assert!(
        stored_media(&pool, row.id).await.is_empty(),
        "the post wears nothing after the clearing edit"
    );
}
