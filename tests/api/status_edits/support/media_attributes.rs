//! `media_attributes` on an edit: alt-text and focal-point fixes of the
//! attachments a post already wears, and the `Update` that federates them.

use wired_be::{
    federation::publisher,
    models::{
        follow::{self, FollowState},
        user,
    },
};

use super::*;

/// Gives the stored attachment `id` the alt text `description`.
async fn describe(pool: &PgPool, id: Uuid, description: &str) {
    sqlx::query("UPDATE media_attachments SET description = $2 WHERE id = $1")
        .bind(id)
        .bind(description)
        .execute(pool)
        .await
        .map(drop)
        .expect("the fixture description writes");
}

/// The stored description and focal point of attachment `id`.
async fn stored_metadata(pool: &PgPool, id: Uuid) -> (Option<String>, Option<f32>, Option<f32>) {
    let row = media_attachment::get_by_id(pool, id)
        .await
        .expect("the attachment reads")
        .expect("the attachment is stored");
    (row.description, row.focus_x, row.focus_y)
}

/// The queued `federator.publish` activities, in insertion order.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// A description sent with the edit replaces the stored one: the answer and
/// a later read show it, a version is superseded, and exactly one `Update`
/// is queued whose attachment `name` is the new text.
#[tokio::test]
async fn an_edit_changes_the_alt_text_and_federates_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let ap_id = format!("{TEST_BASE_URL}/users/alt-editor");
    let created = common::create_local_user_with(&pool, "alt-editor", |mut new| {
        new.ap_id = ap_id.clone();
        new.inbox = Some(user::inbox(&ap_id));
        new.followers_url = Some(user::followers(&ap_id));
        new.private_key = Some("PEM".to_owned());
        new.public_key = Some("PUB".to_owned());
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &created, "write:statuses read:statuses").await;
    let carol = common::create_remote_user(&pool, "carol", "peer.example").await;
    let followed = follow::follow(&pool, TEST_BASE_URL, &carol, &created)
        .await
        .expect("the fixture follow records");
    assert_eq!(followed.state, FollowState::Accepted, "an open account");
    let row = post(&pool, &created, "a picture").await;
    let mid = attach(&pool, &created, &row, 0).await;
    describe(&pool, mid, "wired first alt").await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({
            "status": "a picture (edited)",
            "media_ids": [mid.to_string()],
            "media_attributes": [{"id": mid.to_string(), "description": "wired second alt"}],
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the alt-text edit passes");
    assert_eq!(
        response.json_value()["media_attachments"][0]["description"],
        "wired second alt",
        "the answer carries the new text"
    );
    let read = common::TestRequest::get(&format!("/api/v1/statuses/{}", row.id))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        read.json_value()["media_attachments"][0]["description"],
        "wired second alt",
        "and so does a later read"
    );
    assert_eq!(revisions(&pool, row.id).await.len(), 1, "a version went");

    let activities = published(&pool).await;
    assert_eq!(activities.len(), 1, "one Update is queued");
    assert_eq!(activities[0]["type"], "Update");
    assert_eq!(
        activities[0]["object"]["attachment"][0]["name"], "wired second alt",
        "the federated note names the new alt text"
    );
}

/// An edit that changes only an attachment's alt text still supersedes a
/// version, and the entry's absent parts keep their stored values.
#[tokio::test]
async fn alt_text_alone_is_a_change_and_absent_parts_keep() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "alt-only").await;
    let row = post(&pool, &alice, "caption").await;
    let mid = attach(&pool, &alice, &row, 0).await;
    describe(&pool, mid, "before").await;

    let focus = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "focus": "0.5,-0.5"}]}),
    )
    .await;
    common::assert_answer(&focus, StatusCode::OK, "a focus-only entry passes");
    assert_eq!(
        stored_metadata(&pool, mid).await,
        (Some("before".to_owned()), Some(0.5), Some(-0.5)),
        "the focus moved and the text stayed"
    );

    let text = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "description": "after"}]}),
    )
    .await;
    common::assert_answer(&text, StatusCode::OK, "a text-only entry passes");
    assert_eq!(
        stored_metadata(&pool, mid).await,
        (Some("after".to_owned()), Some(0.5), Some(-0.5)),
        "the text moved and the focus stayed"
    );
    assert_eq!(revisions(&pool, row.id).await.len(), 2, "each was a change");

    let same = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "description": "after"}]}),
    )
    .await;
    common::assert_answer(&same, StatusCode::OK, "a restatement passes");
    assert_eq!(
        revisions(&pool, row.id).await.len(),
        2,
        "restating the stored text records nothing"
    );

    let cleared = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "focus": ""}]}),
    )
    .await;
    common::assert_answer(&cleared, StatusCode::OK, "an empty focus passes");
    assert_eq!(
        stored_metadata(&pool, mid).await,
        (Some("after".to_owned()), None, None),
        "an empty focus clears it"
    );
}

/// An edit without `media_attributes` leaves every description alone.
#[tokio::test]
async fn an_edit_without_attributes_leaves_descriptions_untouched() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "alt-keeper").await;
    let row = post(&pool, &alice, "caption").await;
    let mid = attach(&pool, &alice, &row, 0).await;
    describe(&pool, mid, "kept").await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"status": "new caption", "media_ids": [mid.to_string()]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the edit passes");
    assert_eq!(
        response.json_value()["media_attachments"][0]["description"],
        "kept",
        "the description is what it was"
    );
}

/// An id that is no attachment of the post — unknown, malformed, another
/// account's, or one the edit drops — fails the whole edit with 422
/// `Validation failed` and writes nothing.
#[tokio::test]
async fn an_entry_outside_the_posts_media_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "alt-refuser").await;
    let (bob, _) = editor(&pool, "alt-stranger").await;
    let row = post(&pool, &alice, "caption").await;
    let mine = attach(&pool, &alice, &row, 0).await;
    let theirs = upload(&pool, &bob).await;
    describe(&pool, mine, "stays").await;

    for id in [
        UNKNOWN_ID.to_owned(),
        "nonsense".to_owned(),
        theirs.to_string(),
    ] {
        let response = edit_row(
            &app,
            &token,
            &row,
            json!({
                "status": "must not land",
                "media_attributes": [
                    {"id": mine.to_string(), "description": "must not land"},
                    {"id": id, "description": "x"},
                ],
            }),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a foreign entry refuses the edit",
        );
        assert_eq!(response.json_value()["error"], "Validation failed");
    }
    assert_eq!(
        stored_metadata(&pool, mine).await.0.as_deref(),
        Some("stays"),
        "the refusal rolled the valid entry back"
    );
    assert!(revisions(&pool, row.id).await.is_empty(), "no revision");

    let dropped = edit_row(
        &app,
        &token,
        &row,
        json!({
            "media_ids": [],
            "media_attributes": [{"id": mine.to_string(), "description": "gone"}],
        }),
    )
    .await;
    common::assert_answer(
        &dropped,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an entry for media the edit removes is refused",
    );
}

/// Alt text over `MAX_MEDIA_DESCRIPTION` and a focal point outside `[-1, 1]`
/// are 422s, and a newly claimed upload may be described in the same edit.
#[tokio::test]
async fn limits_apply_and_a_fresh_upload_can_be_described() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "alt-limits").await;
    let row = post(&pool, &alice, "caption").await;
    let mid = attach(&pool, &alice, &row, 0).await;

    let long = "x".repeat(1_501);
    let over = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "description": long}]}),
    )
    .await;
    common::assert_answer(
        &over,
        StatusCode::UNPROCESSABLE_ENTITY,
        "alt text over the limit",
    );
    let wild = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "focus": "2,0"}]}),
    )
    .await;
    common::assert_answer(
        &wild,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a focal point out of range",
    );

    let fresh = upload(&pool, &alice).await;
    let response = edit_row(
        &app,
        &token,
        &row,
        json!({
            "media_ids": [mid.to_string(), fresh.to_string()],
            "media_attributes": [{"id": fresh.to_string(), "description": "brand new"}],
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the new upload is described");
    assert_eq!(
        stored_metadata(&pool, fresh).await.0.as_deref(),
        Some("brand new")
    );
}

/// The form spellings carry the same entries: `media_attributes[][id]=…` and
/// the indexed `media_attributes[0][id]=…`.
#[tokio::test]
async fn form_encoded_entries_work() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "alt-form").await;
    let row = post(&pool, &alice, "caption").await;
    let first = attach(&pool, &alice, &row, 0).await;
    let second = attach(&pool, &alice, &row, 1).await;
    let (first_id, second_id) = (first.to_string(), second.to_string());

    let appended = common::TestRequest::put(&format!("/api/v1/statuses/{}", row.id))
        .bearer(&token)
        .form(&[
            ("media_attributes[][id]", first_id.as_str()),
            ("media_attributes[][description]", "form one"),
            ("media_attributes[][id]", second_id.as_str()),
            ("media_attributes[][description]", "form two"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&appended, StatusCode::OK, "the appended form passes");
    assert_eq!(
        stored_metadata(&pool, first).await.0.as_deref(),
        Some("form one")
    );
    assert_eq!(
        stored_metadata(&pool, second).await.0.as_deref(),
        Some("form two")
    );

    let indexed = common::TestRequest::put(&format!("/api/v1/statuses/{}", row.id))
        .bearer(&token)
        .form(&[
            ("media_attributes[0][id]", first_id.as_str()),
            ("media_attributes[0][description]", "indexed"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&indexed, StatusCode::OK, "the indexed form passes");
    assert_eq!(
        stored_metadata(&pool, first).await.0.as_deref(),
        Some("indexed")
    );
}
