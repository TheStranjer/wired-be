//! WBE-86's `media_ids`: the poster's own unposted uploads, claimed in the
//! order the body sent them from JSON and form bodies alike; more than the
//! cap or one unclaimable id is the 422 with nothing written; a media-only
//! post needs no text; attachments ride every visibility and the submitted
//! `sensitive` flag is kept.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        media_attachment::{self, NewMediaAttachment},
        status::Visibility,
        user::User,
    },
};

use super::support::{
    EMPTY_STATUS, STATUSES, WRITE, attach, created_row, posted, publish, stored_count,
    user_with_token,
};
use crate::common;

// ───────────────────────  POST /api/v1/statuses: media  ──────────────────────

/// 422 body for more submitted `media_ids` than
/// `Config::max_media_attachments` (Pleroma's `validate_attachments_count`).
const TOO_MANY_ATTACHMENTS: &str = "Too many attachments";
/// 422 body for a `media_ids` entry that names no media the poster may
/// claim: an unknown id, another account's upload, a row a status already
/// holds and id text that is no id are all this one refusal.
const MEDIA_NOT_FOUND: &str = "Media not found";

/// An unposted upload by `author` — the row a `media_ids` entry names.
async fn upload(pool: &PgPool, author: &User) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture upload inserts")
    .id
}

/// The stored media row, for the claims and releases a test inspects.
async fn media_row(pool: &PgPool, id: Uuid) -> media_attachment::MediaAttachment {
    media_attachment::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture media is stored")
}

/// The `media_attachments` ids of a creation answer, in answer order.
fn attachment_ids(value: &Value) -> Vec<String> {
    value["media_attachments"]
        .as_array()
        .expect("media_attachments is an array")
        .iter()
        .map(|attachment| {
            attachment["id"]
                .as_str()
                .expect("an attachment id")
                .to_owned()
        })
        .collect()
}

/// A post carrying one media attachment is created, answered as the
/// `Status` entity with the attachment rendered in `media_attachments`
/// (`id`, `type`, `url`), and claims the row for the status at position
/// zero — the poster's own upload, attached.
#[tokio::test]
async fn a_post_with_one_media_attachment_answers_and_claims_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let picture = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({
            "status": "look at this",
            "media_ids": [picture.to_string()],
        }),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(attachment_ids(&value), vec![picture.to_string()]);
    let media = value["media_attachments"].as_array().expect("an array")[0].clone();
    assert_eq!(media["id"], picture.to_string(), "the entity names the row");
    assert_eq!(media["type"], "image", "derived from the stored MIME type");
    assert_eq!(
        media["url"],
        format!("{TEST_BASE_URL}/media/fixture.png"),
        "served at the upload's URL"
    );
    assert_eq!(media["pleroma"]["mime_type"], "image/png");

    let stored = media_row(&pool, picture).await;
    assert_eq!(
        stored.status_id,
        Some(row.id),
        "the upload is now the status's"
    );
    assert_eq!(stored.position, 0, "first in a one-attachment list");
}

/// Multiple attachments render — and are stored by `position` — in the
/// order the body asked for, not the order the uploads were minted in.
#[tokio::test]
async fn media_keeps_the_requested_order_across_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let first = upload(&pool, &alice).await;
    let second = upload(&pool, &alice).await;
    let third = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({
            "status": "a whole gallery",
            "media_ids": [third.to_string(), first.to_string(), second.to_string()],
        }),
    )
    .await;
    let row = created_row(&pool, &value).await;

    assert_eq!(
        attachment_ids(&value),
        vec![third.to_string(), first.to_string(), second.to_string(),],
        "the entity answers the order the client asked for"
    );
    for (position, id) in [(0, third), (1, first), (2, second)] {
        let stored = media_row(&pool, id).await;
        assert_eq!(stored.status_id, Some(row.id), "all three were claimed");
        assert_eq!(stored.position, position, "each at its list position");
    }
}

/// `media_ids[]` form keys (decision 10) arrive as the JSON array does:
/// the same list, in the same order, claimed the same way.
#[tokio::test]
async fn media_ids_form_keys_arrive_like_the_json_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let first = upload(&pool, &alice).await;
    let second = upload(&pool, &alice).await;

    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "from a form"),
            ("media_ids[]", first.to_string().as_str()),
            ("media_ids[]", second.to_string().as_str()),
        ])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a form post with media");
    let value = response.json_value();

    assert_eq!(
        attachment_ids(&value),
        vec![first.to_string(), second.to_string()],
        "the bracketed keys keep their order"
    );
    assert_eq!(media_row(&pool, first).await.position, 0);
    assert_eq!(media_row(&pool, second).await.position, 1);
}

/// More ids than `MAX_MEDIA_ATTACHMENTS` (4 by default) is 422 `Too many
/// attachments` — counted on the submission as sent, before anything about
/// the ids is looked up, and nothing is written.
#[tokio::test]
async fn too_many_attachments_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..5 {
        ids.push(upload(&pool, &alice).await.to_string());
    }

    let response = publish(
        &app,
        Some(&token),
        json!({"status": "greedy", "media_ids": ids}),
    )
    .await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body over the attachment limit",
    );
    common::assert_error(&response, TOO_MANY_ATTACHMENTS, "the refusal wording");
    assert_eq!(stored_count(&pool).await, 0, "and nothing was written");
    for id in &ids {
        let stored = media_row(&pool, Uuid::parse_str(id).expect("an id")).await;
        assert!(stored.status_id.is_none(), "and no media was claimed");
    }
}

/// Every media id the poster may not claim — another account's upload, a
/// row a status already holds, an id no row answers and id text that is no
/// id at all — is one 422 `Media not found`, and none of a refused body's
/// attachable ids get taken along with it.
#[tokio::test]
async fn media_the_poster_cannot_claim_is_media_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    let foreign = upload(&pool, &bob).await;
    let stands =
        common::create_status(&pool, &alice, "an old post stands", Visibility::Public).await;
    let used = attach(&pool, &alice, stands.id).await;
    let unknown = Uuid::now_v7();

    for (what, id) in [
        ("somebody else's upload", foreign.to_string()),
        ("media a status already claims", used.to_string()),
        ("an id no row holds", unknown.to_string()),
        ("id text that is no id", "not-an-id".to_owned()),
    ] {
        let mine = upload(&pool, &alice).await;
        let response = publish(
            &app,
            Some(&token),
            json!({
                "status": "may I borrow it",
                "media_ids": [mine.to_string(), id],
            }),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the refusal for {what}"),
        );
        common::assert_error(
            &response,
            MEDIA_NOT_FOUND,
            &format!("the wording for {what}"),
        );
        assert_eq!(stored_count(&pool).await, 1, "{what}: no second post");
        assert!(
            media_row(&pool, mine).await.status_id.is_none(),
            "{what}: the claimable id rode the refusal out with the rest"
        );
    }

    assert!(
        media_row(&pool, foreign).await.status_id.is_none(),
        "the probes never attached the foreign row"
    );
    assert!(
        media_row(&pool, used).await.status_id.is_some(),
        "and the already-claimed row stayed where it was"
    );
}

/// A body carrying at least one attachment may leave its text empty — the
/// media-only post Pleroma admits — while an empty attachment list is no
/// attachment and the old refusal stands.
#[tokio::test]
async fn a_media_only_post_needs_no_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let picture = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({"status": "   ", "media_ids": [picture.to_string()]}),
    )
    .await;
    let row = created_row(&pool, &value).await;
    assert_eq!(row.raw_text, "", "the post says nothing and stands");
    assert_eq!(value["content"], "", "the empty text renders empty");
    assert_eq!(
        attachment_ids(&value),
        vec![picture.to_string()],
        "and what it has to say is the picture"
    );

    let no_media = publish(&app, Some(&token), json!({"status": "", "media_ids": []})).await;
    common::assert_answer(
        &no_media,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an empty attachment list is no attachment",
    );
    common::assert_error(
        &no_media,
        EMPTY_STATUS,
        "the plain empty-status refusal stands",
    );
}

/// Attachments ride every visibility, Pleroma's `direct` included: media
/// is no reason a private audience cannot post.
#[tokio::test]
async fn every_visibility_can_post_with_media() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for scope in ["public", "unlisted", "private", "direct", "local"] {
        let picture = upload(&pool, &alice).await;
        let value = posted(
            &app,
            &token,
            json!({
                "status": format!("said under {scope}"),
                "visibility": scope,
                "media_ids": [picture.to_string()],
            }),
        )
        .await;
        assert_eq!(value["visibility"], scope, "`{scope}` stands as asked");
        assert_eq!(
            attachment_ids(&value),
            vec![picture.to_string()],
            "`{scope}` carries its attachment"
        );
        assert!(
            media_row(&pool, picture).await.status_id.is_some(),
            "`{scope}` claimed the row"
        );
    }
}

/// The `sensitive` flag a media post sends is the status's own flag and
/// nothing the attachments rewrite: the entity answers it as submitted,
/// whatever the media.
#[tokio::test]
async fn a_media_post_keeps_the_submitted_sensitive_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    let picture = upload(&pool, &alice).await;

    let value = posted(
        &app,
        &token,
        json!({
            "status": "flagged as asked",
            "sensitive": true,
            "media_ids": [picture.to_string()],
        }),
    )
    .await;
    assert_eq!(value["sensitive"], true, "the submitted flag");
    assert!(
        created_row(&pool, &value).await.sensitive,
        "stored as submitted"
    );

    let plain = upload(&pool, &alice).await;
    let off = posted(
        &app,
        &token,
        json!({"status": "unflagged", "sensitive": false, "media_ids": [plain.to_string()]}),
    )
    .await;
    assert_eq!(off["sensitive"], false, "and a submitted false too");
}
