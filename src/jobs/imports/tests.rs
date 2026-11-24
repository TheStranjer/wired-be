//! The import handlers' tests: the per-kind walks, the chunking of every
//! kind, and the discards and retries of an importer that cannot act.

use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use super::*;
use crate::{
    config::{Config, TEST_BASE_URL},
    models::job::Job,
    models::user::{self, NewUser, insert, local_ap_id},
};

/// A handle that neither the database nor a fast-failing `WebFinger`
/// check can resolve (single-label domain), the skip case.
const UNRESOLVABLE: &str = "ghost@invalid";

/// Test state over `pool` with the fixed test config.
fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Test state over a pool that cannot answer: closed before it ever
/// connected, which is how a handler sees a database that went away.
async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    app_state(&pool)
}

/// Inserts a local user named `nickname`.
async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Inserts a mirror of `nickname` on `host`: the shape of a remote
/// account the fetch would store, so a handler entry resolves from the
/// database without any network (the ticket's mock-remote-fixture).
async fn remote_user(pool: &PgPool, nickname: &str, host: &str) -> User {
    let ap_id = format!("https://{host}/users/{nickname}");
    let new = NewUser {
        nickname: format!("{nickname}@{host}"),
        ap_id,
        domain: Some(host.to_owned()),
        name: nickname.to_owned(),
        ..NewUser::default()
    };
    insert(pool, &new)
        .await
        .expect("remote user fixture inserts")
}

/// The handler's attempt info: imports do not depend on the attempt.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// The stored job rows of `kind`, oldest first.
async fn jobs_of(pool: &PgPool, kind: &str) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(kind)
        .fetch_all(pool)
        .await
        .expect("the job rows read")
}

/// The follow row of the pair, which the test expects to exist.
async fn relation(pool: &PgPool, follower: &User, target: &User) -> follow::Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the relationship exists")
}

/// Whether the pair has any follow row.
async fn relation_exists(pool: &PgPool, follower: &User, target: &User) -> bool {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .is_some()
}

/// The follow handler follows resolvable accts — a stored local and a
/// stored remote one, the mock-remote-fixture case — writes the
/// `Show boosts` column it was given, and logs-and-skips an acct
/// nothing resolves.
#[tokio::test]
async fn follows_apply_the_list_and_skip_unresolvable_accts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob", "remote.example").await;
    let carol = local_user(&pool, "carol").await;
    let args = json!({
        "user_id": alice.id,
        "accts": [bob.nickname, carol.nickname, UNRESOLVABLE],
        "options": [{"show_reblogs": false}, {}, {"show_reblogs": true}],
    });

    let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_eq!(outcome, Ok(()), "a skipped entry never fails the job");
    let bob_follow = relation(&pool, &alice, &bob).await;
    assert!(!bob_follow.show_reblogs, "the False column reaches the row");
    assert_eq!(
        bob_follow.state,
        follow::FollowState::Pending,
        "a mirror of a remote account waits for that instance, as the \
             interactive follow does"
    );
    assert!(
        relation(&pool, &alice, &carol).await.show_reblogs,
        "an entry without a value keeps the default"
    );
    assert!(
        follow::is_following(&pool, &alice, &carol).await.unwrap(),
        "the local acct's follow was granted on the spot"
    );
    assert!(
        jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
        "a list shorter than the chunk leaves no remainder job"
    );
}

/// The follow handler processes the first 50 entries and re-enqueues
/// the remainder (in order, with the aligned options) due 5 s later.
#[tokio::test]
async fn follows_chunk_at_fifty_and_reenqueue_the_remainder() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let mut names: Vec<String> = Vec::new();
    let mut users = Vec::new();
    for i in 0..60 {
        let name = format!("user{i}@chunk.example");
        users.push(remote_user(&pool, &format!("user{i}"), "chunk.example").await);
        names.push(name);
    }
    let options: Vec<Value> = names.iter().map(|_| json!({})).collect();
    let args = json!({"user_id": alice.id, "accts": names, "options": options});

    let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_eq!(outcome, Ok(()), "the first chunk runs clean");
    for user in users.iter().take(50) {
        assert!(
            relation_exists(&pool, &alice, user).await,
            "the first chunk followed {}",
            user.nickname
        );
    }
    for user in users.iter().skip(50) {
        assert!(
            !relation_exists(&pool, &alice, user).await,
            "the remainder is left for the next chunk: {}",
            user.nickname
        );
    }
    let pending = jobs_of(&pool, KIND_FOLLOWS).await;
    assert_eq!(pending.len(), 1, "exactly one remainder job");
    let next = &pending[0];
    assert_eq!(next.queue, QUEUE_BACKGROUND, "on the background queue");
    assert_eq!(
        next.args
            .get("accts")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(10),
        "the remainder carries the last ten accts, in order: {}",
        next.args
    );
    assert_eq!(
        next.args["accts"][0],
        json!("user50@chunk.example"),
        "and it starts exactly where this run stopped"
    );
    assert_eq!(
        next.args["options"].as_array().map_or_default(Vec::len),
        10,
        "the options travel with the accts"
    );
    assert!(
        next.run_at > Utc::now(),
        "the chunk is throttled into the future: {}",
        next.run_at
    );
    assert!(
        next.run_at < Utc::now() + IMPORT_CHUNK_DELAY + IMPORT_CHUNK_DELAY,
        "the throttle is the ticket's 5 s, not a backoff: {}",
        next.run_at
    );
}

/// Inserts `count` local users named `user0..` and returns them with
/// their accts, so a chunk test resolves from the database alone.
async fn local_users(pool: &PgPool, count: usize) -> (Vec<User>, Vec<String>) {
    let mut users = Vec::new();
    let mut names = Vec::new();
    for i in 0..count {
        let user = local_user(pool, &format!("user{i}")).await;
        names.push(user.nickname.clone());
        users.push(user);
    }
    (users, names)
}

/// Asserts exactly one `kind` job is queued carrying the last `left`
/// accts of `names`, due about [`IMPORT_CHUNK_DELAY`] from now.
async fn assert_one_remainder(pool: &PgPool, kind: &str, names: &[String], left: usize) {
    let pending = jobs_of(pool, kind).await;
    assert_eq!(pending.len(), 1, "exactly one remainder `{kind}` job");
    let next = &pending[0];
    assert_eq!(next.queue, QUEUE_BACKGROUND, "on the background queue");
    let expected = &names[names.len() - left..];
    assert_eq!(
        next.args["accts"],
        json!(expected),
        "the remainder is the unprocessed tail, in order"
    );
    assert!(
        next.run_at > Utc::now() + TimeDelta::seconds(3),
        "throttled about 5 s out: {}",
        next.run_at
    );
    assert!(
        next.run_at < Utc::now() + IMPORT_CHUNK_DELAY + TimeDelta::seconds(2),
        "and not a backoff: {}",
        next.run_at
    );
}

/// A 120-entry block import blocks the first 50 and queues one
/// `imports.blocks` job with the other 70.
#[tokio::test]
async fn blocks_chunk_at_fifty_and_reenqueue_the_remainder() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let (users, names) = local_users(&pool, 120).await;
    let args = job_args(alice.id, &names, &[]);

    let outcome = handle_blocks(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_eq!(outcome, Ok(()), "the first chunk runs clean");
    for (i, user) in users.iter().enumerate() {
        assert_eq!(
            block::is_blocking(&pool, alice.id, user.id).await.unwrap(),
            i < IMPORT_CHUNK,
            "only the first chunk is blocked: {}",
            user.nickname
        );
    }
    assert_one_remainder(&pool, KIND_BLOCKS, &names, 70).await;
    assert!(
        jobs_of(&pool, KIND_MUTES).await.is_empty()
            && jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
        "the remainder keeps the import's kind"
    );
}

/// A 120-entry mute import mutes the first 50 and queues one
/// `imports.mutes` job with the other 70.
#[tokio::test]
async fn mutes_chunk_at_fifty_and_reenqueue_the_remainder() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let (users, names) = local_users(&pool, 120).await;
    let args = job_args(alice.id, &names, &[]);

    let outcome = handle_mutes(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_eq!(outcome, Ok(()), "the first chunk runs clean");
    for (i, user) in users.iter().enumerate() {
        assert_eq!(
            mute::is_muting(&pool, alice.id, user.id).await.unwrap(),
            i < IMPORT_CHUNK,
            "only the first chunk is muted: {}",
            user.nickname
        );
    }
    assert_one_remainder(&pool, KIND_MUTES, &names, 70).await;
    assert!(
        jobs_of(&pool, KIND_BLOCKS).await.is_empty()
            && jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
        "the remainder keeps the import's kind"
    );
}

/// A list of exactly one chunk, or fewer, enqueues nothing for any kind.
#[tokio::test]
async fn a_list_within_one_chunk_enqueues_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let (_users, names) = local_users(&pool, IMPORT_CHUNK).await;
    let args = job_args(alice.id, &names, &[]);
    let state = app_state(&pool);

    assert_eq!(
        handle_blocks(state.clone(), FIRST_ATTEMPT, args.clone()).await,
        Ok(())
    );
    assert_eq!(handle_mutes(state, FIRST_ATTEMPT, args).await, Ok(()));

    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(queued, 0, "a full chunk leaves no remainder job");
}

/// The second chunk of the ticket's own throttling: running the
/// re-enqueued job applies its remainder and enqueues nothing more.
#[tokio::test]
async fn the_remainder_chunk_completes_the_import() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob", "remote.example").await;
    let args = json!({
        "user_id": alice.id,
        "accts": [bob.nickname],
        "options": [{"show_reblogs": true}],
    });

    let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_eq!(outcome, Ok(()), "a small job is its own whole chunk");
    assert!(
        relation_exists(&pool, &alice, &bob).await,
        "and it followed"
    );
    assert!(
        jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
        "no remainder"
    );
}

/// The blocks handler blocks resolvable accts — local and mirrored
/// remote — logs-and-skips the rest, and is idempotent across runs.
#[tokio::test]
async fn blocks_apply_to_resolvable_accts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob", "remote.example").await;
    let args = json!({
        "user_id": alice.id,
        "accts": [bob.nickname, alice.nickname, UNRESOLVABLE],
        "options": [],
    });
    let state = app_state(&pool);

    let outcome = handle_blocks(state.clone(), FIRST_ATTEMPT, args.clone()).await;
    assert_eq!(outcome, Ok(()), "the self refusal and the skip pass");
    assert!(
        block::is_blocking(&pool, alice.id, bob.id).await.unwrap(),
        "the stored remote acct is blocked"
    );
    assert!(
        !block::is_blocking(&pool, alice.id, alice.id).await.unwrap(),
        "importing yourself is skipped, not written"
    );

    let rerun = handle_blocks(state, FIRST_ATTEMPT, args).await;
    assert_eq!(
        rerun,
        Ok(()),
        "a second run changes nothing and fails nowhere"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM blocks")
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(rows, 1, "idempotent: still exactly one block row");
}

/// The mutes handler mutes resolvable accts with the Mastodon
/// defaults (notifications hidden, indefinite) and skips what it
/// cannot resolve.
#[tokio::test]
async fn mutes_apply_to_resolvable_accts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob", "remote.example").await;
    let args = json!({
        "user_id": alice.id,
        "accts": [bob.nickname, UNRESOLVABLE],
        "options": [],
    });

    let outcome = handle_mutes(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_eq!(outcome, Ok(()), "the unresolvable entry is a skip");
    assert!(
        mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "the stored remote acct is muted"
    );
    let expires: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM mutes WHERE muter_id = $1")
            .bind(alice.id)
            .fetch_one(&pool)
            .await
            .expect("the mute row reads");
    assert_eq!(expires, None, "an import mute is indefinite");
}

/// A blocked pair is Pleroma's logged skip, not a job failure: the
/// follow of an account that blocks the importer keeps the walk going.
#[tokio::test]
async fn a_blocked_target_is_skipped_not_fatal() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let _blocked = block::block(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .expect("bob blocks alice");
    let carol = local_user(&pool, "carol").await;
    let args = json!({
        "user_id": alice.id,
        "accts": [bob.nickname, carol.nickname],
        "options": [],
    });

    let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_eq!(outcome, Ok(()), "the refusal is logged and skipped");
    assert!(
        !follow::is_following(&pool, &alice, &bob).await.unwrap(),
        "the blocking account was not followed"
    );
    assert!(
        follow::is_following(&pool, &alice, &carol).await.unwrap(),
        "and the list walk continued to the next entry"
    );
}

/// Arguments that do not fit [`ImportArgs`] are permanent: discard.
#[tokio::test]
async fn unusable_arguments_discard_the_job() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();

    let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, json!({"accts": []})).await;

    match outcome {
        Err(JobError::Discard(detail)) => assert!(
            detail.contains("unusable"),
            "the discard says why: {detail}"
        ),
        Ok(()) => panic!("a missing user_id must not complete the job"),
        Err(other) => panic!("a missing user_id must discard, not {other}"),
    }
}

/// A user the database no longer has is permanent: discard.
#[tokio::test]
async fn a_vanished_user_discards_the_job() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let args = job_args(Uuid::now_v7(), &[], &[]);

    let outcome = handle_blocks(app_state(&pool), FIRST_ATTEMPT, args).await;

    match outcome {
        Err(JobError::Discard(detail)) => assert!(
            detail.contains("no longer exists"),
            "the discard says why: {detail}"
        ),
        Ok(()) => panic!("a vanished user must not complete the job"),
        Err(other) => panic!("a vanished user must discard, not {other}"),
    }
}

/// Asserts `outcome` is the "can no longer act" discard.
fn assert_cannot_act(outcome: Result<(), JobError>) {
    match outcome {
        Err(JobError::Discard(detail)) => assert!(
            detail.contains("can no longer act"),
            "the discard says why: {detail}"
        ),
        Ok(()) => panic!("an inactive importer must not complete the job"),
        Err(other) => panic!("an inactive importer must discard, not {other}"),
    }
}

/// A deleted importer's follow import is discarded: no follow row, and
/// no next chunk even when the list exceeds one chunk.
#[tokio::test]
async fn a_deleted_importer_follows_nothing_and_ends_the_chain() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let mut names: Vec<String> = Vec::new();
    let mut users = Vec::new();
    for i in 0..60 {
        users.push(remote_user(&pool, &format!("user{i}"), "chunk.example").await);
        names.push(format!("user{i}@chunk.example"));
    }
    user::mark_deleted(&pool, alice.id)
        .await
        .expect("alice is marked deleted");
    let args = job_args(alice.id, &names, &[]);

    let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

    assert_cannot_act(outcome);
    for target in &users {
        assert!(
            !relation_exists(&pool, &alice, target).await,
            "no follow row for {}",
            target.nickname
        );
    }
    assert!(
        jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
        "no next chunk is enqueued"
    );
}

/// A deactivated importer's block and mute imports are discarded and
/// write nothing.
#[tokio::test]
async fn a_deactivated_importer_blocks_and_mutes_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob", "remote.example").await;
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .map(drop)
        .expect("alice is deactivated");
    let args = job_args(alice.id, std::slice::from_ref(&bob.nickname), &[]);
    let state = app_state(&pool);

    assert_cannot_act(handle_blocks(state.clone(), FIRST_ATTEMPT, args.clone()).await);
    assert_cannot_act(handle_mutes(state, FIRST_ATTEMPT, args).await);

    assert!(
        !block::is_blocking(&pool, alice.id, bob.id).await.unwrap(),
        "no block was written"
    );
    assert!(
        !mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "no mute was written"
    );
}

/// An importer awaiting staff approval cannot act either.
#[tokio::test]
async fn an_unapproved_importer_is_discarded() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    sqlx::query("UPDATE users SET approval_pending = true WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .map(drop)
        .expect("alice awaits approval");

    let outcome = handle_blocks(
        app_state(&pool),
        FIRST_ATTEMPT,
        job_args(alice.id, &[], &[]),
    )
    .await;

    assert_cannot_act(outcome);
}

/// An active importer still applies its entries.
#[tokio::test]
async fn an_active_importer_still_applies_entries() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob", "remote.example").await;
    let carol = remote_user(&pool, "carol", "remote.example").await;
    let dave = remote_user(&pool, "dave", "remote.example").await;
    let state = app_state(&pool);
    let args_for = |target: &User| job_args(alice.id, std::slice::from_ref(&target.nickname), &[]);

    assert_eq!(
        handle_follows(state.clone(), FIRST_ATTEMPT, args_for(&bob)).await,
        Ok(())
    );
    assert_eq!(
        handle_blocks(state.clone(), FIRST_ATTEMPT, args_for(&carol)).await,
        Ok(())
    );
    assert_eq!(
        handle_mutes(state, FIRST_ATTEMPT, args_for(&dave)).await,
        Ok(())
    );

    assert!(
        relation_exists(&pool, &alice, &bob).await,
        "the follow was written"
    );
    assert!(
        block::is_blocking(&pool, alice.id, carol.id).await.unwrap(),
        "the block was written"
    );
    assert!(
        mute::is_muting(&pool, alice.id, dave.id).await.unwrap(),
        "the mute was written"
    );
}

/// A database that cannot answer is transient: retry, so the import
/// resumes with its idempotent writes once the database is back.
#[tokio::test]
async fn an_unreachable_database_retries_the_job() {
    let args = json!({"user_id": Uuid::now_v7().to_string(), "accts": [], "options": []});
    let state = broken_state().await;

    let outcome = handle_mutes(state, FIRST_ATTEMPT, args).await;

    match outcome {
        Err(JobError::Retry(detail)) => assert!(
            detail.contains("import user read failed"),
            "the retry states what failed: {detail}"
        ),
        Ok(()) => panic!("a closed pool must not read as a successful import"),
        Err(other) => panic!("a closed pool is retryable, not {other}"),
    }
}

/// All three kinds are registered with the worker's registry, each
/// bound to this module's handler.
#[tokio::test]
async fn the_kinds_are_registered() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let registry = crate::job_registry();
    let alice = local_user(&pool, "alice").await;
    let args = job_args(alice.id, &[], &[]);
    for kind in [KIND_FOLLOWS, KIND_BLOCKS, KIND_MUTES] {
        let handler = registry
            .get(kind)
            .unwrap_or_else(|| panic!("`{kind}` is registered"));
        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, args.clone()).await;
        assert_eq!(outcome, Ok(()), "the registered handler for `{kind}` runs");
    }
}
