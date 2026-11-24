//! Instance deletion: the staged sweeps, the queue and the refusals.

use axum::http::StatusCode;
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    jobs::deletion::KIND_DELETE_REMOTE,
    models::{
        follow, instance,
        user::{User, mark_deleted},
    },
};

use super::support::{ALL_ADMIN, INSTANCES, admin, federation_app, log_entry, queued_args};
use crate::common;

/// The instance-deletion world: a local follower of two fresh mirrors
/// and a pre-stamped third one on the doomed domain, a mirror on a kept
/// domain, and a row in the ledger for each host.
#[derive(Debug)]
struct Sweep {
    /// The local account following one doomed mirror.
    alice: User,
    /// The first doomed mirror — the one followed.
    bob: User,
    /// The second doomed mirror.
    carol: User,
    /// The doomed mirror already marked deleted.
    dave: User,
    /// A mirror of another domain.
    erin: User,
}

/// Builds the [`Sweep`] fixture.
async fn sweep_world(pool: &PgPool) -> Sweep {
    let alice = common::create_local_user(pool, "alice").await;
    let bob = common::create_remote_user(pool, "bob", "sweep.example").await;
    let carol = common::create_remote_user(pool, "carol", "sweep.example").await;
    let dave = common::create_remote_user(pool, "dave", "sweep.example").await;
    let erin = common::create_remote_user(pool, "erin", "keep.example").await;
    mark_deleted(pool, dave.id)
        .await
        .expect("the pre-stamped mirror stands deleted");
    let row = follow::follow(pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("alice follows bob");
    let _granted = follow::accept(pool, row.id)
        .await
        .expect("the follow is accepted");
    instance::touch(pool, "sweep.example")
        .await
        .expect("the doomed host has a row");
    instance::touch(pool, "keep.example")
        .await
        .expect("the kept host has a row");
    Sweep {
        alice,
        bob,
        carol,
        dave,
        erin,
    }
}

/// Deleting a REMOTE instance: every fresh mirror of the domain is
/// marked deleted and rides its own background `users.delete_remote`
/// sweep — queued in one batch — while local rows, another domain's
/// mirrors and rows already marked stand aside; the `instances` row goes
/// and the answer is the domain as a JSON string.
#[tokio::test]
async fn deleting_an_instance_stages_the_sweeps_and_clears_the_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let world = sweep_world(&pool).await;

    let deleted = common::TestRequest::delete(&format!("{INSTANCES}/sweep.example"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::OK, "deleting an instance");
    assert_eq!(
        deleted.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(
        deleted.json::<String>(),
        "sweep.example",
        "the domain as a JSON string, Pleroma's body"
    );

    let sweeps = queued_args(&pool, KIND_DELETE_REMOTE).await;
    let mut doomed: Vec<String> = sweeps
        .iter()
        .map(|args| args["user_id"].as_str().expect("a user id").to_owned())
        .collect();
    doomed.sort();
    let mut expected = vec![world.bob.id.to_string(), world.carol.id.to_string()];
    expected.sort();
    assert_eq!(
        doomed, expected,
        "one job per fresh mirror — the pre-stamped row is nobody to re-delete"
    );
    for id in [world.bob.id, world.carol.id] {
        let gone = common::reload_user(&pool, id).await;
        assert!(!gone.is_active.get(), "{id} is deactivated");
        assert!(gone.deleted_at.is_some(), "{id} carries the deletion stamp");
    }
    let pre = common::reload_user(&pool, world.dave.id).await;
    assert!(
        !pre.is_active.get() && pre.deleted_at.is_some(),
        "the pre-stamped mirror stands deleted, queued nothing again"
    );
    let untouched = [
        common::reload_user(&pool, world.alice.id).await,
        common::reload_user(&pool, world.erin.id).await,
    ];
    assert!(
        untouched
            .iter()
            .all(|u| u.is_active.get() && u.deleted_at.is_none()),
        "the local account and the kept domain's mirror stand untouched"
    );

    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
        .bind("sweep.example")
        .fetch_one(&pool)
        .await
        .expect("the row count reads");
    assert_eq!(rows, 0, "the instances row is cleared");
    let (sentence, data) = log_entry(&pool, "delete_instance").await;
    assert_eq!(
        sentence, "@root deleted instance: sweep.example",
        "the write is logged"
    );
    assert_eq!(data["target"], "sweep.example", "with the domain as target");
}

/// The staged sweeps complete on the queue: running the jobs clears the
/// relationships the doomed mirrors name, steps the live follower's
/// counters, and leaves the kept domain's mirror's follow standing.
#[tokio::test]
async fn the_instance_deletion_sweeps_run_on_the_queue() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", "admin:write:federation").await;
    let world = sweep_world(&pool).await;
    let kept = follow::follow(&pool, TEST_BASE_URL, &world.alice, &world.erin)
        .await
        .expect("alice also follows the kept mirror");
    let _granted = follow::accept(&pool, kept.id)
        .await
        .expect("that follow stands accepted");

    let deleted = common::TestRequest::delete(&format!("{INSTANCES}/Sweep.Example"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(
        &deleted,
        StatusCode::OK,
        "the mixed-case domain deletes too",
    );
    assert_eq!(
        deleted.json::<String>(),
        "sweep.example",
        "the answer carries the domain as the ledger keys it"
    );
    let queued_elsewhere = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND queue <> 'background'",
    )
    .bind(KIND_DELETE_REMOTE)
    .fetch_one(&pool)
    .await
    .expect("the queue reads");
    assert_eq!(
        queued_elsewhere, 0,
        "every sweep sits on the background queue"
    );

    let ran = common::run_all_jobs(&state).await;
    assert_eq!(ran, 2, "both sweeps ran");
    assert!(
        follow::get(&pool, world.alice.id, world.bob.id)
            .await
            .expect("the follow reads")
            .is_none(),
        "alice's follow of the swept mirror is gone"
    );
    let alice = common::reload_user(&pool, world.alice.id).await;
    assert_eq!(
        alice.following_count, 1,
        "only the swept follow stepped back"
    );
    assert!(
        follow::get(&pool, world.alice.id, world.erin.id)
            .await
            .expect("the follow reads")
            .is_some(),
        "the kept mirror's follow stands"
    );
    let kept_rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
        .bind("keep.example")
        .fetch_one(&pool)
        .await
        .expect("the row count reads");
    assert_eq!(kept_rows, 1, "and the neighbouring ledger row stands");
    let done = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND state = 'completed'",
    )
    .bind(KIND_DELETE_REMOTE)
    .fetch_one(&pool)
    .await
    .expect("the job states read");
    assert_eq!(done, 2, "the queue records both sweeps complete");
}

/// This instance's own domain is not a deletion candidate — Pleroma's
/// refusal, verbatim, in any spelling of the host — and an unknown peer
/// domain deletes cleanly as a no-op.
#[tokio::test]
async fn the_local_domain_refuses_deletion_and_a_stranger_is_a_noop() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", "admin:write:federation").await;

    for spelling in ["test.example", "TEST.Example"] {
        let path = format!("{INSTANCES}/{spelling}");
        let refused = common::TestRequest::delete(&path)
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the local domain");
        common::assert_error(
            &refused,
            "Can't delete the local instance",
            "Pleroma's refusal wording",
        );
    }

    let stranger = common::TestRequest::delete(&format!("{INSTANCES}/never-seen.example"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&stranger, StatusCode::OK, "a domain with no rows");
    assert_eq!(
        stranger.json::<String>(),
        "never-seen.example",
        "the answer is still the domain"
    );
    assert!(
        queued_args(&pool, KIND_DELETE_REMOTE).await.is_empty(),
        "and nothing was queued"
    );
}

// --- the emoji reload ---------------------------------------------------------------------
