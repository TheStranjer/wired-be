//! `ingest_note`'s own tests: the plain fold writes a remote status with
//! zeroed counters, the `uri` dedupe restates a stored row and refuses a
//! tombstone, a deactivated author and an unparseable document are turned
//! away, and the `ACCEPT_UNSOLICITED_PUBLIC` gate drops an unfollowed
//! author's public post unless a local account is mentioned or answered.

use sqlx::PgPool;

use super::*;
use crate::{
    config::Config,
    models::{
        status::NewStatus,
        user::{self, NewUser},
    },
    services::ingest::test_support::{app_state, author, note, state_with_config},
};

/// Counts the `statuses` rows stored under object uri `n`.
async fn rows_for(pool: &PgPool, n: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(format!("https://remote.example/objects/{n}"))
        .fetch_one(pool)
        .await
        .expect("the row count reads")
}

#[tokio::test]
async fn ingesting_a_note_writes_a_remote_status() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let outcome = ingest_note(&state, &bob, &note("ingest-1"))
        .await
        .expect("the fixture note ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("a first-seen note must be created, got {outcome:?}");
    };
    assert!(!row.local, "the row is remote");
    assert_eq!(
        row.uri, "https://remote.example/objects/ingest-1",
        "the object id is the uri"
    );
    assert_eq!(row.user_id, bob.id, "the author owns the row");
    assert_eq!(row.reblogs_count, 0, "the counters stay zero");
    assert_eq!(row.favourites_count, 0, "both of them");
    assert_eq!(
        row.inserted_at.to_rfc3339(),
        "2024-02-01T10:00:00+00:00",
        "the published time is the creation time"
    );
}

#[tokio::test]
async fn a_second_create_of_the_same_object_answers_the_stored_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let first = ingest_note(&state, &bob, &note("ingest-dup"))
        .await
        .expect("the first create ingests");
    let second = ingest_note(&state, &bob, &note("ingest-dup"))
        .await
        .expect("the second create is not an error");

    let IngestOutcome::Existing(row) = second else {
        panic!("a repeat create restates the stored row, got {second:?}");
    };
    let IngestOutcome::Created(created) = first else {
        panic!("the first create writes");
    };
    assert_eq!(row.id, created.id, "and it restates the same row");
    assert_eq!(
        rows_for(&pool, "ingest-dup").await,
        1,
        "one row for the two creates"
    );
}

#[tokio::test]
async fn a_create_of_a_tombstoned_uri_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let IngestOutcome::Created(row) = ingest_note(&state, &bob, &note("ingest-dead"))
        .await
        .expect("the first create ingests")
    else {
        panic!("the first create writes");
    };
    let _tombstone = status::soft_delete(&pool, row.id)
        .await
        .expect("the delete tombstones");
    let outcome = ingest_note(&state, &bob, &note("ingest-dead"))
        .await
        .expect("the re-import is not an error");

    assert_eq!(
        outcome,
        IngestOutcome::Ignored("tombstoned"),
        "a deleted post is never re-imported"
    );
}

#[tokio::test]
async fn a_deactivated_authors_post_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let created = author(&pool).await;
    let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(created.id)
        .execute(&pool)
        .await
        .expect("the deactivation writes");
    // Re-read the row as the dispatcher's `load_signer` hands it: the
    // live one, with `is_active` already false.
    let bob = user::get_by_id(&pool, created.id)
        .await
        .expect("the author reads back")
        .expect("the author is stored");

    let outcome = ingest_note(&state, &bob, &note("ingest-inactive"))
        .await
        .expect("the refusal is not an error");

    assert_eq!(
        outcome,
        IngestOutcome::Ignored("deactivated author"),
        "a retired mirror ingests nothing"
    );
    assert_eq!(
        rows_for(&pool, "ingest-inactive").await,
        0,
        "and no row stands"
    );
}

#[tokio::test]
async fn an_unparseable_object_is_a_permanent_failure() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let err = ingest_note(
        &state,
        &bob,
        &json!({"id": "https://remote.example/objects/x", "type": "ChatMessage"}),
    )
    .await
    .expect_err("a chat message is not this ticket's note");

    assert!(
        matches!(err, FederationError::Permanent(_)),
        "no retry can fix the type, and it must not ask for one: {err}"
    );
}

#[tokio::test]
async fn the_unsolicited_gate_drops_public_notes_from_unfollowed_authors() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = gating_state(&pool);
    let bob = author(&pool).await;
    let alice = user::insert(
        &pool,
        &NewUser::local("alice", &user::local_ap_id(&state.config.base_url, "alice")),
    )
    .await
    .expect("the local account inserts");

    let dropped = ingest_note(&state, &bob, &note("ingest-unsolicited"))
        .await
        .expect("the refusal is not an error");
    assert_eq!(
        dropped,
        IngestOutcome::Ignored("unsolicited public"),
        "off, an unfollowed author's public post is dropped"
    );

    let mentioned = json!({
        "id": "https://remote.example/objects/ingest-mentioned-local",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>hello alice</p>",
        "tag": [{ "type": "Mention", "href": alice.ap_id.clone(), "name": "@alice" }],
    });
    let outcome = ingest_note(&state, &bob, &mentioned)
        .await
        .expect("the local mention resolves");
    assert!(
        matches!(outcome, IngestOutcome::Created(_)),
        "a post that mentions a local user passes the gate anyway, got {outcome:?}"
    );

    let direct = json!({
        "id": "https://remote.example/objects/ingest-gated-private",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://remote.example/users/bob/followers"],
        "content": "<p>followers only</p>",
    });
    let to_followers = ingest_note(&state, &bob, &direct)
        .await
        .expect("the followers note ingests");
    assert!(
        matches!(to_followers, IngestOutcome::Created(_)),
        "a followers-addressed post was aimed at somebody here, got {to_followers:?}"
    );

    let open = follow::follow(&pool, &state.config.base_url, &alice, &bob)
        .await
        .expect("the follow writes");
    let _accepted = follow::accept(&pool, open.id)
        .await
        .expect("the follow is accepted");
    let solicited = ingest_note(&state, &bob, &note("ingest-solicited"))
        .await
        .expect("the post-follow fold runs");
    assert!(
        matches!(solicited, IngestOutcome::Created(_)),
        "once a local account follows, the author's public posts pass, got {solicited:?}"
    );
}

#[tokio::test]
async fn a_reply_to_a_local_status_passes_the_unsolicited_gate() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = gating_state(&pool);
    let bob = author(&pool).await;
    let alice = user::insert(
        &pool,
        &NewUser::local("alice", &user::local_ap_id(&state.config.base_url, "alice")),
    )
    .await
    .expect("the local account inserts");
    let local_post = status::insert(
        &pool,
        &state.config.base_url,
        &NewStatus::local(&alice, "the local root"),
    )
    .await
    .expect("the local status writes");

    let child = json!({
        "id": "https://remote.example/objects/ingest-reply-local",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>answering alice</p>",
        "inReplyTo": local_post.uri,
    });
    let outcome = ingest_note(&state, &bob, &child)
        .await
        .expect("the reply ingests");
    let IngestOutcome::Created(row) = outcome else {
        panic!("a post answering a local status passes the gate, got {outcome:?}");
    };
    assert_eq!(
        row.in_reply_to_id,
        Some(local_post.id),
        "and keeps its link to the local parent"
    );
}

/// The state with `ACCEPT_UNSOLICITED_PUBLIC` off: the gate under test.
fn gating_state(pool: &PgPool) -> AppState {
    let config = Config::from_lookup(|key| match key {
        crate::config::DATABASE_URL_VAR => Some(crate::config::TEST_DATABASE_URL.to_owned()),
        crate::config::BASE_URL_VAR => Some(crate::config::TEST_BASE_URL.to_owned()),
        crate::config::ACCEPT_UNSOLICITED_PUBLIC_VAR => Some("false".to_owned()),
        _ => None,
    })
    .expect("the gating test config is valid");
    state_with_config(pool, config)
}

/// A direct note by `bob` named `n`: nobody addressed, as a `bcc` delivery
/// reads once the sender stripped the field.
fn bcc_note(n: &str) -> Value {
    json!({
        "id": format!("https://remote.example/objects/{n}"),
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": [],
        "content": "<p>for your eyes only</p>",
    })
}

/// Inserts the local account `nickname`.
async fn local_user(state: &AppState, nickname: &str) -> User {
    user::insert(
        &state.db,
        &NewUser::local(
            nickname,
            &user::local_ap_id(&state.config.base_url, nickname),
        ),
    )
    .await
    .expect("the local account inserts")
}

/// The `user_id`s mentioned by `status_id`.
async fn mention_rows(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM status_mentions WHERE status_id = $1")
        .bind(status_id)
        .fetch_all(pool)
        .await
        .expect("the mention rows read")
}

#[tokio::test]
async fn a_direct_note_delivered_to_a_personal_inbox_mentions_its_owner() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;

    let outcome = ingest_note_delivered(&state, &bob, &bcc_note("ingest-bcc"), Some(&alice))
        .await
        .expect("the delivery ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("a first-seen note must be created, got {outcome:?}");
    };
    assert_eq!(row.visibility, Visibility::Direct, "nobody is addressed");
    assert_eq!(
        mention_rows(&pool, row.id).await,
        vec![alice.id],
        "the inbox owner is mentioned"
    );
    assert!(
        crate::models::status_visibility::can_view(&pool, &row, Some(&alice))
            .await
            .expect("the visibility check runs"),
        "so the one person it was delivered to can see it"
    );
}

#[tokio::test]
async fn a_shared_inbox_delivery_adds_no_mention() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;

    let outcome = ingest_note_delivered(&state, &bob, &bcc_note("ingest-shared"), None)
        .await
        .expect("the delivery ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("a first-seen note must be created, got {outcome:?}");
    };
    assert!(
        mention_rows(&pool, row.id).await.is_empty(),
        "no owner, no mention"
    );
    assert!(
        !crate::models::status_visibility::can_view(&pool, &row, Some(&alice))
            .await
            .expect("the visibility check runs"),
        "and nobody is told about it"
    );
}

#[tokio::test]
async fn an_owner_already_mentioned_is_not_duplicated() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;
    let mut object = bcc_note("ingest-twice");
    object["tag"] = json!([{ "type": "Mention", "href": alice.ap_id.clone(), "name": "@alice" }]);

    let outcome = ingest_note_delivered(&state, &bob, &object, Some(&alice))
        .await
        .expect("the delivery ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("a first-seen note must be created, got {outcome:?}");
    };
    assert_eq!(
        mention_rows(&pool, row.id).await,
        vec![alice.id],
        "one row, not two"
    );
}

#[tokio::test]
async fn a_non_direct_note_delivered_to_a_personal_inbox_adds_no_mention() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;

    let outcome = ingest_note_delivered(&state, &bob, &note("ingest-public-owner"), Some(&alice))
        .await
        .expect("the delivery ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("a first-seen note must be created, got {outcome:?}");
    };
    assert!(
        mention_rows(&pool, row.id).await.is_empty(),
        "a public post reaches the owner through follows, not a mention"
    );
}
