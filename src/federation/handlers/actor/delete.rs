//! Applying a `Delete` of a remote actor: retiring the mirror and
//! queueing the sweep.

use tracing::debug;

use crate::{
    federation::dispatch::{ActivityView, FederationError, Outcome},
    jobs::{
        QUEUE_BACKGROUND,
        deletion::{KIND_DELETE_REMOTE, job_args},
    },
    models::{
        job::{NewJob, enqueue},
        user::{self, User},
    },
    state::AppState,
};

/// The body of [`delete_actor`]: resolve the object's id, and retire the
/// account it names when that account is the sender itself.
///
/// The mark and the queueing are two statements, so a failure between them
/// leaves an inactive account whose sweep arrives with the retry: the
/// activity's ledger row goes back for a transient failure, and folding the
/// activity again writes the same mark and queues an idempotent sweep.
pub(super) async fn apply_delete(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object_id) = view.object.id() else {
        return Ok(Outcome::Ignored("a Delete whose object names no id"));
    };
    if object_id != view.actor {
        debug!(
            object_id,
            "a Delete whose object is not the sending actor's own id; ignoring"
        );
        return Ok(Outcome::Ignored(
            "a Delete whose object is not the actor's own id",
        ));
    }
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Delete of an account this instance owns",
        ));
    }
    user::mark_deleted(&state.db, signer.id)
        .await
        .map_err(|err| {
            FederationError::transient("could not store the remote actor's deletion mark", &err)
        })?;
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_BACKGROUND,
            kind: KIND_DELETE_REMOTE,
            args: job_args(signer.id),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await
    .map_err(|err| {
        FederationError::transient("could not store the remote actor's delete sweep", &err)
    })?;
    debug!(
        account = %signer.nickname,
        queued = queued.is_some(),
        "a remote account deleted itself"
    );
    Ok(Outcome::Handled)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{
        delete_actor,
        test_support::{
            ALICE, accepted_follow, app_state, assert_handled, broken_state, fresh_user, ignored,
            local_user, mirror, mirror_post, person, queued_sweep, view,
        },
    };
    use super::*;
    use crate::{
        jobs::AttemptInfo,
        models::{follow, status},
    };

    /// A `Delete` of the sender's own id retires the mirror — inactive, with
    /// `deleted_at` stamped — and queues the sweep on the `background` queue
    /// the ticket names; running that sweep takes the mirror's statuses and
    /// the local follows of it.
    #[tokio::test]
    async fn a_delete_of_its_own_account_deactivates_the_mirror_and_queues_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = mirror(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "bob").await;
        let _following = accepted_follow(&pool, &bob, &alice).await;
        let posted = mirror_post(&pool, &alice, "farewell").await;

        let gone = delete_actor(
            state.clone(),
            view("Delete", "delete-1", ALICE, json!(ALICE)),
            alice,
        )
        .await;
        assert_handled(&gone);

        let retired = fresh_user(&pool, alice_id).await;
        assert!(!retired.is_active.get(), "the mirror is deactivated");
        assert!(retired.deleted_at.is_some(), "and marked deleted");
        assert_eq!(retired.ap_id, ALICE, "the row itself stays for the refs");
        let job = queued_sweep(&pool, alice_id)
            .await
            .expect("the sweep was queued");
        assert_eq!(job.0, QUEUE_BACKGROUND, "on the background queue");
        assert_eq!(job.1, KIND_DELETE_REMOTE, "as the remote sweep");
        assert_eq!(job.2, json!({"user_id": alice_id}), "carrying the account");

        let sweep = crate::job_registry()
            .get(KIND_DELETE_REMOTE)
            .expect("the sweep kind is registered");
        let attempt = AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        };
        sweep(state, attempt, job_args(alice_id))
            .await
            .expect("the queued sweep runs");

        assert!(
            status::get_by_id(&pool, posted.id)
                .await
                .expect("the status reads")
                .is_none(),
            "the departed account's post is no longer a live status"
        );
        assert!(
            follow::get(&pool, bob.id, alice_id)
                .await
                .expect("the follow reads")
                .is_none(),
            "and bob stops following it"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.following_count,
            0,
            "with his counter stepped down"
        );
    }

    /// The other spellings of a self-deletion — a `Tombstone` object, and an
    /// embedded actor document — resolve through the object's id to the same
    /// retirement.
    #[tokio::test]
    async fn a_self_deletion_arrives_as_an_id_a_tombstone_or_a_document() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = mirror(&pool).await;
        let alice_id = alice.id;

        let tombstoned = delete_actor(
            state.clone(),
            view(
                "Delete",
                "delete-2",
                ALICE,
                json!({"type": "Tombstone", "id": ALICE}),
            ),
            alice,
        )
        .await;
        assert_handled(&tombstoned);
        let gone = fresh_user(&pool, alice_id).await;
        assert!(
            gone.deleted_at.is_some() && !gone.is_active.get(),
            "the tombstone spelled its deletion"
        );

        let embedded = delete_actor(
            state.clone(),
            view("Delete", "delete-3", ALICE, person()),
            gone,
        )
        .await;
        assert_handled(&embedded);
    }

    /// An id that is not the sender's own account is not this handler's
    /// business: a status id is the status-delete ticket's route, another
    /// account's id is nobody's to delete here, an object that names no id
    /// resolves to nothing, and an account this instance owns is deleted by
    /// its own endpoint. All ignored, nothing queued.
    #[tokio::test]
    async fn a_delete_of_anything_else_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice_id = mirror(&pool).await.id;
        let ours = local_user(&pool, "alice-here").await;
        let ours_id = ours.id;

        let status_delete = delete_actor(
            state.clone(),
            view(
                "Delete",
                "other-1",
                ALICE,
                json!("https://peer.example/posts/gone"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(status_delete).contains("not the actor's own id"),
            "a status id is the status ticket's route"
        );

        let foreign_actor = delete_actor(
            state.clone(),
            view(
                "Delete",
                "other-2",
                ALICE,
                json!("https://peer.example/users/mallory"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(foreign_actor).contains("not the actor's own id"),
            "another account's id names no account this delivery may retire"
        );

        let idless = delete_actor(
            state.clone(),
            view("Delete", "other-3", ALICE, json!({"type": "Tombstone"})),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(idless).contains("names no id"),
            "an object with no id resolves to nothing"
        );

        let local = delete_actor(
            state.clone(),
            view("Delete", "other-4", &ours.ap_id, json!(ours.ap_id.clone())),
            ours,
        )
        .await;
        assert!(
            ignored(local).contains("this instance owns"),
            "a local account is not federation's to delete"
        );

        let kept = fresh_user(&pool, alice_id).await;
        assert!(kept.is_active.get(), "none of them touched the mirror");
        assert!(
            kept.deleted_at.is_none(),
            "and none of them marked anything deleted"
        );
        assert!(
            fresh_user(&pool, ours_id).await.deleted_at.is_none(),
            "the local row stayed live"
        );
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = $1")
            .bind(KIND_DELETE_REMOTE)
            .fetch_one(&pool)
            .await
            .expect("the job queue reads");
        assert_eq!(queued, 0, "an ignored Delete queues nothing");
    }

    /// A `Delete` whose database cannot answer is transient, and the account
    /// stays live for the retry to find.
    #[tokio::test]
    async fn a_delete_against_a_dead_database_is_transient() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice_id = mirror(&pool).await.id;

        let outcome = delete_actor(
            broken_state().await,
            view("Delete", "dead-2", ALICE, json!(ALICE)),
            fresh_user(&pool, alice_id).await,
        )
        .await;

        assert!(
            matches!(outcome, Err(FederationError::Transient(_))),
            "the mark failing asks for another attempt: {outcome:?}"
        );
        assert!(
            fresh_user(&pool, alice_id).await.is_active.get(),
            "the healthy pool still has a live account to retry for"
        );
    }
}
