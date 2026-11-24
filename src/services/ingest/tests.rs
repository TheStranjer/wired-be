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
        custom_emoji::{self, NewCustomEmoji},
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

/// `bob`'s public note named `n` announcing `:ralph:` at `image_url`.
fn emoji_note(n: &str, image_url: &str) -> Value {
    let mut object = note(n);
    object["tag"] = json!([{
        "type": "Emoji",
        "name": ":ralph:",
        "icon": {"type": "Image", "url": image_url},
    }]);
    object
}

/// Counts the `custom_emojis` rows of any shortcode.
async fn emoji_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM custom_emojis")
        .fetch_one(pool)
        .await
        .expect("the emoji count reads")
}

#[tokio::test]
async fn a_dropped_unsolicited_post_writes_no_emoji_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = gating_state(&pool);
    let bob = author(&pool).await;

    let dropped = ingest_note(
        &state,
        &bob,
        &emoji_note("ingest-emoji-dropped", "https://remote.example/e.png"),
    )
    .await
    .expect("the refusal is not an error");

    assert_eq!(
        dropped,
        IngestOutcome::Ignored("unsolicited public"),
        "the gate drops the post"
    );
    assert_eq!(emoji_rows(&pool).await, 0, "and its emoji never lands");
}

#[tokio::test]
async fn a_post_refreshes_a_stored_emoji_image_but_keeps_its_moderation_flags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let stored = NewCustomEmoji {
        shortcode: "ralph".to_owned(),
        domain: Some("remote.example".to_owned()),
        image_url: "https://remote.example/old.png".to_owned(),
        category: Some("pack".to_owned()),
        tags: vec!["dog".to_owned()],
        visible_in_picker: false,
        disabled: true,
        ..NewCustomEmoji::default()
    };
    let _seeded = custom_emoji::upsert(&pool, &stored)
        .await
        .expect("the emoji seeds");

    let outcome = ingest_note(
        &state,
        &bob,
        &emoji_note("ingest-emoji-kept", "https://remote.example/new.png"),
    )
    .await
    .expect("the post ingests");
    assert!(
        matches!(outcome, IngestOutcome::Created(_)),
        "the post is created, got {outcome:?}"
    );

    let rows =
        custom_emoji::get_by_shortcodes(&pool, &["ralph".to_owned()], Some("remote.example"))
            .await
            .expect("the emoji reads");
    let [row] = rows.as_slice() else {
        panic!("one emoji row expected, got {}", rows.len());
    };
    assert_eq!(
        row.image_url, "https://remote.example/new.png",
        "the image url follows the newest post"
    );
    assert!(row.disabled, "a disabled emoji stays disabled");
    assert!(!row.visible_in_picker, "picker visibility is kept");
    assert_eq!(row.category.as_deref(), Some("pack"), "so is the pack");
    assert_eq!(row.tags, ["dog"], "and the tags");
}

#[tokio::test]
async fn a_post_with_an_unknown_emoji_stores_it_with_the_defaults() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let _outcome = ingest_note(
        &state,
        &bob,
        &emoji_note("ingest-emoji-new", "https://remote.example/new.png"),
    )
    .await
    .expect("the post ingests");

    let rows =
        custom_emoji::get_by_shortcodes(&pool, &["ralph".to_owned()], Some("remote.example"))
            .await
            .expect("the emoji reads");
    let [row] = rows.as_slice() else {
        panic!("one emoji row expected, got {}", rows.len());
    };
    assert_eq!(row.image_url, "https://remote.example/new.png", "stored");
    assert!(!row.disabled, "enabled by default");
    assert!(row.visible_in_picker, "visible by default");
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

/// A tagless direct note by `bob` named `n` addressed to `to`.
fn addressed_note(n: &str, to: &[&str]) -> Value {
    json!({
        "id": format!("https://remote.example/objects/{n}"),
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": to,
        "tag": [],
        "content": "<p>for you</p>",
    })
}

#[tokio::test]
async fn a_tagless_direct_note_addressed_to_a_local_user_mentions_them() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;
    let object = addressed_note("ingest-audience-dm", &[alice.ap_id.as_str()]);

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("the note ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("a first-seen note must be created, got {outcome:?}");
    };
    assert_eq!(
        row.visibility,
        Visibility::Direct,
        "no public, no followers"
    );
    assert_eq!(
        mention_rows(&pool, row.id).await,
        vec![alice.id],
        "the addressed local user is a silent mention"
    );
    assert!(
        crate::models::status_visibility::can_view(&pool, &row, Some(&alice))
            .await
            .expect("the visibility check runs"),
        "so she can read it"
    );
}

#[tokio::test]
async fn an_unstored_remote_recipient_is_ignored_and_not_fetched() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let object = addressed_note(
        "ingest-audience-remote",
        &["https://elsewhere.example/users/zed"],
    );

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("an unknown recipient is no failure");

    let IngestOutcome::Created(row) = outcome else {
        panic!("the note still ingests, got {outcome:?}");
    };
    assert!(
        mention_rows(&pool, row.id).await.is_empty(),
        "nobody is mentioned"
    );
    let stored = user::get_by_ap_id(&pool, "https://elsewhere.example/users/zed")
        .await
        .expect("the lookup runs");
    assert!(stored.is_none(), "the recipient was never fetched");
}

#[tokio::test]
async fn a_remote_account_in_the_audience_is_not_mentioned() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let carol = user::insert(
        &pool,
        &NewUser {
            nickname: "carol@remote.example".to_owned(),
            ap_id: "https://remote.example/users/carol".to_owned(),
            local: user::Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote account inserts");
    let object = addressed_note("ingest-audience-stored-remote", &[carol.ap_id.as_str()]);

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("the note ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("the note ingests, got {outcome:?}");
    };
    assert!(
        mention_rows(&pool, row.id).await.is_empty(),
        "only local accounts are linked"
    );
}

#[tokio::test]
async fn a_public_note_addressing_a_local_user_in_cc_links_them_once() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let alice = local_user(&state, "alice").await;
    let mut object = note("ingest-audience-cc");
    object["cc"] = json!([
        "https://remote.example/users/bob/followers",
        alice.ap_id.clone()
    ]);
    object["tag"] = json!([{ "type": "Mention", "href": alice.ap_id.clone(), "name": "@alice" }]);

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("the note ingests");

    let IngestOutcome::Created(row) = outcome else {
        panic!("a first-seen note must be created, got {outcome:?}");
    };
    assert_eq!(row.visibility, Visibility::Public, "public in to");
    assert_eq!(
        mention_rows(&pool, row.id).await,
        vec![alice.id],
        "linked once though both the tag and cc name her"
    );

    let mut untagged = note("ingest-audience-cc-only");
    untagged["cc"] = json!([alice.ap_id.clone()]);
    let IngestOutcome::Created(second) = ingest_note(&state, &bob, &untagged)
        .await
        .expect("the untagged note ingests")
    else {
        panic!("the untagged note writes");
    };
    assert_eq!(
        mention_rows(&pool, second.id).await,
        vec![alice.id],
        "cc alone links her too"
    );
}

/// `bob`'s public note named `n`, answering the object id `parent` that no
/// stored status has: the reply the backfill is queued for.
fn orphan_reply(n: &str, parent: &str) -> Value {
    let mut object = note(n);
    object["inReplyTo"] = json!(format!("https://remote.example/objects/{parent}"));
    object
}

/// The `depth` args of every `federator.backfill` job queued for `status_id`.
async fn queued_backfill_depths(pool: &PgPool, status_id: Uuid) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>("SELECT args->'depth' FROM jobs WHERE unique_key = $1")
        .bind(backfill_key(status_id))
        .fetch_all(pool)
        .await
        .expect("the job read")
}

#[tokio::test]
async fn the_backfill_job_is_queued_at_the_depth_the_note_was_ingested_at() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let IngestOutcome::Created(row) = ingest_note_at_depth(
        &state,
        &bob,
        &orphan_reply("ingest-depth-3", "ingest-depth-4"),
        3,
    )
    .await
    .expect("the reply ingests") else {
        panic!("a first-seen note is created");
    };

    assert_eq!(
        queued_backfill_depths(&pool, row.id).await,
        vec![json!(3)],
        "the job carries the depth, not zero"
    );
}

#[tokio::test]
async fn ingest_note_queues_the_backfill_at_depth_zero() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let IngestOutcome::Created(row) =
        ingest_note(&state, &bob, &orphan_reply("ingest-depth-0", "ingest-up"))
            .await
            .expect("the reply ingests")
    else {
        panic!("a first-seen note is created");
    };

    assert_eq!(
        queued_backfill_depths(&pool, row.id).await,
        vec![json!(0)],
        "a delivered reply starts the walk at zero"
    );
}

#[tokio::test]
async fn enqueue_backfill_queues_nothing_at_or_past_the_depth_limit() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mut config = Config::for_tests();
    config.fetch.limits.thread_fetch_depth = 2;
    let state = state_with_config(&pool, config);
    let bob = author(&pool).await;

    for (name, depth, queued) in [
        ("ingest-limit-below", 1, vec![json!(1)]),
        ("ingest-limit-at", 2, Vec::new()),
        ("ingest-limit-past", 9, Vec::new()),
    ] {
        let IngestOutcome::Created(row) =
            ingest_note_at_depth(&state, &bob, &orphan_reply(name, "ingest-up"), depth)
                .await
                .expect("the reply ingests")
        else {
            panic!("a first-seen note is created");
        };
        assert!(
            row.in_reply_to_ap_id.is_some(),
            "{name} keeps its deferred parent"
        );
        assert_eq!(
            queued_backfill_depths(&pool, row.id).await,
            queued,
            "{name}: depth {depth} against a limit of 2"
        );
    }
}
