//! The note document machinery the status hooks share: the live note as
//! [`crate::federation::note`] renders it, wrapped in the `Create` or the
//! `Update` [`NoteActivity`] names, and the gate on whether a row is this
//! instance's to announce as a note at all.
//!
//! Both arms are read by [`super::statuses`] (the post going out) and
//! [`super::withdrawals`] (the copy coming down), which is why they live
//! here rather than beside either hook.

use serde_json::Value;
use tracing::warn;
use uuid::Uuid;

use super::load_user;
use crate::{
    federation::note,
    models::{
        status::{Status, Visibility},
        user::User,
    },
    state::AppState,
    util::id::new_id,
};

/// The two shapes [`note_document`] wraps a status' live note into: the
/// `Create` that first announces it and the `Update` that re-drafts it,
/// which differ only in the wrapper they put on the same document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NoteActivity {
    /// The status' `Create`: the note wrapped by
    /// [`note::create_activity_json`] under the deterministic
    /// `{BASE_URL}/activities/{status id}` id.
    Create,
    /// The edited status' `Update`: the note wrapped by
    /// [`note::update_activity_json`] under a freshly minted activity id.
    Update,
}

/// The status' live note, wrapped in the lifecycle activity `kind` names,
/// and `None` (logged) when there is nothing to announce: the row or its
/// author died between the write and this read, or the row wears a
/// visibility [`note::note_json`] refuses to render.
pub(super) async fn note_document(
    state: &AppState,
    status_id: Uuid,
    kind: NoteActivity,
) -> Option<Value> {
    let ctx = match note::load_note_context(&state.db, &state.config, status_id).await {
        Ok(ctx) => ctx,
        Err(err) => {
            warn!(
                %err,
                status = %status_id,
                "note federation could not read the status document"
            );
            return None;
        }
    };
    let note = note::note_json(&ctx)?;
    let document = match kind {
        NoteActivity::Create => note::create_activity_json(&note, &ctx.status, &state.config),
        NoteActivity::Update => note::update_activity_json(&note, new_id(), &state.config),
    };
    Some(document)
}

/// The author of a status this instance may announce as a note, `None` when
/// the row is not its business: a mirrored row (its origin sent the
/// `Create`), a reblog row (decision 6: its activity is the `Announce`
/// [`after_reblog_changed`] sends), a `local`-visibility post (which never
/// left the database), a gone author, an author that is not one of our own,
/// or an author switched off — a deactivated account's new post stays inside
/// this instance, the way the ingest fold refuses to store a live post for
/// one.
///
/// [`after_reblog_changed`]: super::after_reblog_changed
pub(super) async fn note_publisher(state: &AppState, status: &Status) -> Option<User> {
    if !federates_as_note(status) {
        return None;
    }
    let author = load_user(state, status.user_id).await?;
    (author.local.get() && author.is_active.get()).then_some(author)
}

/// Whether a status row is the kind this instance sends note activities
/// for: one it owns, a plain post rather than a boost, and a visibility
/// that reaches past this instance.
pub(super) const fn federates_as_note(status: &Status) -> bool {
    status.local && status.reblog_of_id.is_none() && !matches!(status.visibility, Visibility::Local)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    use crate::services::federation_out::{
        after_status_created, after_status_deleted, after_status_edited,
        test_support::{
            accepted_follow, app_state, job_count, local, local_post, publish_jobs, remote,
            the_single_activity,
        },
    };

    /// Switches an account off the way a deactivation or suspension does.
    async fn deactivate(pool: &PgPool, account: &User) {
        let updated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(account.id)
            .execute(pool)
            .await
            .expect("the deactivation fixture writes");
        assert_eq!(
            updated.rows_affected(),
            1,
            "the account is there to switch off"
        );
    }

    /// `local` visibility is the outright refusal, in every direction: none
    /// of the three lifecycle hooks queues a job for such a post, and none
    /// of them expands the followers collection either.
    #[tokio::test]
    async fn a_local_visibility_note_federates_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let hidden = local_post(&pool, &alice, "instance only", Visibility::Local).await;
        let state = app_state(&pool);

        after_status_created(&state, &hidden).await;
        after_status_edited(&state, &hidden).await;
        after_status_deleted(&state, &hidden).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local-visibility post moves no queue at all"
        );
    }

    /// An account that is switched off posts nothing and corrects nothing:
    /// both arms of the announce side refuse it. The withdrawal of a post it
    /// made while it was live still goes out — the peers that hold a copy
    /// have to learn it is gone, which is the whole point of the `Delete`.
    #[tokio::test]
    async fn a_switched_off_author_announces_no_new_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let posted = local_post(&pool, &alice, "before the freeze", Visibility::Public).await;
        deactivate(&pool, &alice).await;
        let state = app_state(&pool);

        after_status_created(&state, &posted).await;
        after_status_edited(&state, &posted).await;
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a deactivated account's post neither goes out nor is re-drafted"
        );

        after_status_deleted(&state, &posted).await;
        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["type"], "Delete",
            "the withdrawal is not held back by the flag"
        );
    }
}
