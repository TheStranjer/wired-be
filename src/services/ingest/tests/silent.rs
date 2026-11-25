//! Addressing in `to`/`cc` is access, not a mention: a local account named
//! only there keeps its access link but gets no notification and no
//! standing with the unsolicited-post gate.

use super::*;
use crate::{models::mention, test_db::test_db};

/// The `mention` notifications stored for `user_id`.
async fn mention_notifications(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM notifications WHERE user_id = $1 AND type = 'mention'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("the notification count reads")
}

/// The `silent` flag of `user_id`'s link to `status_id`.
async fn link_is_silent(pool: &PgPool, status_id: Uuid, user_id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT silent FROM status_mentions WHERE status_id = $1 AND user_id = $2",
    )
    .bind(status_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("the link row reads")
}

#[tokio::test]
async fn a_local_account_named_only_in_cc_gets_no_mention_notification() {
    let db = test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;
    let mut object = note("silent-cc-only");
    object["cc"] = json!([alice.ap_id.clone()]);

    let IngestOutcome::Created(row) = ingest_note(&state, &bob, &object)
        .await
        .expect("the note ingests")
    else {
        panic!("the note is stored");
    };

    assert_eq!(mention_rows(&pool, row.id).await, vec![alice.id]);
    assert!(
        link_is_silent(&pool, row.id, alice.id).await,
        "a silent link"
    );
    assert_eq!(
        mention_notifications(&pool, alice.id).await,
        0,
        "no mention"
    );
    let shown = mention::for_statuses(&pool, &[row.id])
        .await
        .expect("the mentions read");
    assert!(shown.is_empty(), "the entity's mentions skip silent links");
}

#[tokio::test]
async fn a_direct_note_addressed_to_a_local_account_keeps_thread_access_without_a_notification() {
    let db = test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;
    let object = addressed_note("silent-direct", &[alice.ap_id.as_str()]);

    let IngestOutcome::Created(row) = ingest_note(&state, &bob, &object)
        .await
        .expect("the note ingests")
    else {
        panic!("the note is stored");
    };

    assert_eq!(
        mention_notifications(&pool, alice.id).await,
        0,
        "no mention"
    );
    assert!(
        crate::models::status_visibility::can_view(&pool, &row, Some(&alice))
            .await
            .expect("the visibility check runs"),
        "she can still read it"
    );
    let access = mention::access_for_statuses(&pool, &[row.id])
        .await
        .expect("the access links read");
    assert_eq!(
        access.get(&row.id).map(Vec::len),
        Some(1),
        "the thread roster still names her"
    );
}

#[tokio::test]
async fn the_unsolicited_gate_drops_a_note_naming_a_local_account_only_in_cc() {
    let db = test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = gating_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;
    let mut object = note("silent-gated");
    object["cc"] = json!([alice.ap_id.clone()]);

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("the refusal is not an error");

    assert_eq!(outcome, IngestOutcome::Ignored("unsolicited public"));
    assert_eq!(rows_for(&pool, "silent-gated").await, 0, "nothing stored");
}

#[tokio::test]
async fn a_mention_tag_still_notifies_and_passes_the_gate() {
    let db = test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = gating_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;
    let mut object = note("silent-tagged");
    object["cc"] = json!([alice.ap_id.clone()]);
    object["tag"] = json!([{ "type": "Mention", "href": alice.ap_id.clone(), "name": "@alice" }]);

    let IngestOutcome::Created(row) = ingest_note(&state, &bob, &object)
        .await
        .expect("the note ingests")
    else {
        panic!("a real mention passes the gate");
    };

    assert!(
        !link_is_silent(&pool, row.id, alice.id).await,
        "a real link"
    );
    assert_eq!(mention_notifications(&pool, alice.id).await, 1, "notified");
}

#[tokio::test]
async fn a_delivery_owner_is_a_silent_link_on_a_direct_note() {
    let db = test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;

    let IngestOutcome::Created(row) =
        ingest_note_delivered(&state, &bob, &bcc_note("silent-owner"), Some(&alice))
            .await
            .expect("the delivery ingests")
    else {
        panic!("the note is stored");
    };

    assert!(link_is_silent(&pool, row.id, alice.id).await, "silent");
    assert_eq!(
        mention_notifications(&pool, alice.id).await,
        0,
        "no mention"
    );
}
