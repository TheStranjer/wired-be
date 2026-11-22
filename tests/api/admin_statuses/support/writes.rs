//! The status update and delete routes.

use super::*;

// --- PUT /statuses/{id} ----------------------------------------------------------------

/// The scope write moves both fields, answers the re-scoped entity, leaves
/// the text and `edited_at` where they were, and logs Pleroma's sentence
/// naming the parts that moved.
#[tokio::test]
async fn update_moves_both_fields_and_logs_the_change() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_upd_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_upd_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    let answer = rescoped(
        &app,
        &token,
        row.id,
        json!({"sensitive": true, "visibility": "private"}),
    )
    .await;
    assert_eq!(
        answer["visibility"], "private",
        "the entity answers the new scope"
    );
    assert_eq!(answer["sensitive"], json!(true), "and the new flag");

    let moved = stored(&pool, row.id).await;
    assert_eq!(moved.visibility, Visibility::Private, "the row agrees");
    assert!(moved.sensitive, "and so it does with the flag");
    assert_eq!(moved.content, row.content, "the text is where it was");
    assert_eq!(moved.edited_at, None, "a scope change is no content edit");
    assert_eq!(
        moved.uri, row.uri,
        "and the object id it was federated under is reserved"
    );

    assert_eq!(
        log_lines(&pool, "status_update").await,
        vec![(
            "as_upd_root".to_owned(),
            format!(
                "@as_upd_root updated status ##{}, set sensitive: 'true', visibility: 'private'",
                row.id
            )
        )],
        "one entry, Pleroma's status_update sentence"
    );
}

/// One field at a time, and neither: the column the body does not present
/// keeps its value, and an empty body is Pleroma's fall-through that still
/// logs and still answers the entity.
#[tokio::test]
async fn update_moves_only_the_fields_the_body_presents() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_part_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_part_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    let folded = rescoped(&app, &token, row.id, json!({"sensitive": "true"})).await;
    assert_eq!(
        folded["sensitive"],
        json!(true),
        "the string spelling counts"
    );
    assert_eq!(folded["visibility"], "public", "and the visibility holds");

    let private = rescoped(&app, &token, row.id, json!({"visibility": "private"})).await;
    assert_eq!(
        private["visibility"], "private",
        "the next write moves the other field"
    );
    assert_eq!(
        private["sensitive"],
        json!(true),
        "and the flag the first write set still stands"
    );

    let bare = rescoped(&app, &token, row.id, json!({})).await;
    assert_eq!(
        bare["visibility"], "private",
        "an empty body names nothing to move"
    );
    assert_eq!(
        log_lines(&pool, "status_update").await,
        vec![
            (
                "as_part_root".to_owned(),
                format!(
                    "@as_part_root updated status ##{}, set sensitive: 'true'",
                    row.id
                )
            ),
            (
                "as_part_root".to_owned(),
                format!(
                    "@as_part_root updated status ##{}, set visibility: 'private'",
                    row.id
                )
            ),
            (
                "as_part_root".to_owned(),
                format!(
                    "@as_part_root updated status ##{}, set visibility: ''",
                    row.id
                )
            ),
        ],
        "one entry per write, each naming what it moved, and Pleroma's \
         fall-through sentence for the one that moved nothing"
    );
}

/// A `visibility` the column would not hold is the ticket's 422 with the row
/// left exactly as it was; an id that is no post is the 404; and a
/// `sensitive` that is no boolean is the body extractor's own 422.
#[tokio::test]
async fn update_refuses_an_unusable_visibility_and_a_miss() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_bad_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_bad_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    for visibility in ["everything", "Public", ""] {
        let refused = refused_put(&app, &token, row.id, json!({"visibility": visibility})).await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("{visibility} is no visibility"),
        );
        common::assert_error(&refused, INVALID_VISIBILITY, "the refusal body");
    }
    assert_eq!(
        stored(&pool, row.id).await.visibility,
        Visibility::Public,
        "and no refusal wrote anything"
    );
    assert!(
        log_lines(&pool, "status_update").await.is_empty(),
        "nor did one log"
    );

    let missing = refused_put(&app, &token, Uuid::now_v7(), json!({"sensitive": true})).await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "an id that is no post");
    common::assert_error(&missing, NOT_FOUND, "an id that is no post");

    let unusable = refused_put(&app, &token, row.id, json!({"sensitive": "maybe"})).await;
    common::assert_answer(
        &unusable,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a boolean that is no boolean",
    );
}

/// A post of this instance is corrected in the network's copy: the write
/// queues the `Update` of the re-drafted note. A mirrored row belongs to its
/// origin and queues nothing at all.
#[tokio::test]
async fn update_publishes_an_update_for_a_local_post_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_fed_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_fed_alice").await;
    let stranger = common::create_remote_user(&pool, "fedstranger", "remote.example").await;
    let own = post(&pool, &alice, Visibility::Public).await;
    let theirs = mirror_of(&pool, &stranger, "remote.example", "edit").await;

    let _quiet = rescoped(&app, &token, theirs.id, json!({"visibility": "unlisted"})).await;
    assert!(
        published(&pool).await.is_empty(),
        "a mirrored post is not this instance's to correct"
    );

    let _loud = rescoped(&app, &token, own.id, json!({"visibility": "unlisted"})).await;
    let activities = published(&pool).await;
    assert_eq!(activities.len(), 1, "the local edit is announced once");
    assert_eq!(activities[0]["type"], "Update", "as the correction it is");
    assert_eq!(
        activities[0]["object"]["id"], own.uri,
        "and it names the same reserved object uri"
    );
    assert_eq!(
        activities[0]["object"]["to"],
        json!([format!("{TEST_BASE_URL}/users/as_fed_alice/followers")]),
        "addressed as the re-scoped note states it: an unlisted post is \
         written to its author's followers, not to the public"
    );
    assert_eq!(
        activities[0]["object"]["cc"],
        json!([PUBLIC]),
        "and the public address rides the carbon copy, which is what sets \
         unlisted apart from public"
    );
}

/// A local post whose visibility is the never-federated `local` has no
/// document to correct: the write lands, the entity answers, and the queue
/// stays empty.
#[tokio::test]
async fn update_of_a_local_visibility_post_publishes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_quiet_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_quiet_alice").await;
    let row = post(&pool, &alice, Visibility::Local).await;

    let answer = rescoped(&app, &token, row.id, json!({"sensitive": true})).await;
    assert_eq!(
        answer["visibility"], "local",
        "the scope it keeps is answered"
    );
    assert!(
        published(&pool).await.is_empty(),
        "and a local-visibility post has no Update to send"
    );
}

// --- DELETE /statuses/{id} -------------------------------------------------------------

/// The admin withdrawal is the shared deletion service: `200 {}`, the
/// tombstone, the `status_delete` entry and the federated `Delete` — and the
/// 404 for the second strike at the same post.
#[tokio::test]
async fn delete_takes_the_post_down_and_reports_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_del_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_del_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    let answer = removed(&app, &token, row.id).await;
    common::assert_answer(&answer, StatusCode::OK, "the staff delete");
    assert_eq!(
        answer.json_value(),
        json!({}),
        "Pleroma's `json(conn, %{{}})` answer"
    );

    assert!(
        stored(&pool, row.id).await.deleted_at.is_some(),
        "the row is a tombstone now"
    );
    assert_eq!(
        log_lines(&pool, "status_delete").await,
        vec![(
            "as_del_root".to_owned(),
            format!("@as_del_root deleted status ##{}", row.id)
        )],
        "the moderation entry names the withdrawal"
    );
    let activities = published(&pool).await;
    assert_eq!(activities.len(), 1, "the network is told once");
    assert_eq!(activities[0]["type"], "Delete", "with the withdrawal it is");

    let again = removed(&app, &token, row.id).await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "the second strike");
    common::assert_error(&again, NOT_FOUND, "the second strike");
    assert_eq!(
        log_lines(&pool, "status_delete").await.len(),
        1,
        "a refusal logs nothing, and the withdrawal was logged once"
    );
}
