//! The history renders each version's own alt text and focal point, not the
//! attachments' current ones (WBE-800).

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

/// The alt text of the first attachment of every history entry, in order.
fn descriptions_of(body: &Value) -> Vec<Value> {
    body.as_array()
        .expect("the history is an array")
        .iter()
        .map(|entry| entry["media_attachments"][0]["description"].clone())
        .collect()
}

/// An alt-text-only `PUT` leaves the earlier versions wearing the text they
/// had, so the history reads first, second, current — in order.
#[tokio::test]
async fn alt_text_edits_show_each_versions_own_description() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "alt-history").await;
    let row = post(&pool, &alice, "a picture").await;
    let mid = attach(&pool, &alice, &row, 0).await;
    describe(&pool, mid, "first alt").await;

    for text in ["second alt", "third alt"] {
        let response = edit_row(
            &app,
            &token,
            &row,
            json!({"media_attributes": [{"id": mid.to_string(), "description": text}]}),
        )
        .await;
        common::assert_answer(&response, StatusCode::OK, "the alt-text edit passes");
    }

    let read = history(&app, Some(&token), &row.id.to_string()).await;
    common::assert_answer(&read, StatusCode::OK, "the history reads");
    assert_eq!(
        descriptions_of(&read.json_value()),
        vec![json!("first alt"), json!("second alt"), json!("third alt")],
        "each version wears the alt text it was written with"
    );
    let stored = revisions(&pool, row.id).await;
    assert_eq!(
        stored[0].media_descriptions,
        Some(vec![Some("first alt".to_owned())]),
        "the snapshot is aligned with the stored media ids"
    );
}

/// A focal-point move shows in the history of the versions before it too.
#[tokio::test]
async fn focus_edits_show_each_versions_own_focal_point() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "focus-history").await;
    let row = post(&pool, &alice, "a picture").await;
    let mid = attach(&pool, &alice, &row, 0).await;

    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "focus": "0.5,-0.5"}]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the focus edit passes");

    let read = history(&app, Some(&token), &row.id.to_string()).await;
    let body = read.json_value();
    assert_eq!(
        body[0]["media_attachments"][0]["meta"].get("focus"),
        None,
        "the first version had no focal point"
    );
    assert_eq!(
        body[1]["media_attachments"][0]["meta"]["focus"],
        json!({"x": 0.5, "y": -0.5}),
        "the current one wears the new focal point"
    );
}

/// A revision recorded before the snapshot columns existed (`NULL`) renders
/// the attachment's current description.
#[tokio::test]
async fn an_old_revision_without_a_snapshot_renders_the_current_description() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = editor(&pool, "alt-legacy").await;
    let row = post(&pool, &alice, "a picture").await;
    let mid = attach(&pool, &alice, &row, 0).await;
    describe(&pool, mid, "before").await;
    let response = edit_row(
        &app,
        &token,
        &row,
        json!({"media_attributes": [{"id": mid.to_string(), "description": "after"}]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the alt-text edit passes");
    sqlx::query(
        "UPDATE status_edits
            SET media_descriptions = NULL, media_focus_x = NULL, media_focus_y = NULL
          WHERE status_id = $1",
    )
    .bind(row.id)
    .execute(&pool)
    .await
    .map(drop)
    .expect("the legacy shape writes");

    let read = history(&app, Some(&token), &row.id.to_string()).await;
    common::assert_answer(&read, StatusCode::OK, "the history reads");
    assert_eq!(
        descriptions_of(&read.json_value()),
        vec![json!("after"), json!("after")],
        "a revision without a snapshot falls back to the stored text"
    );
}
