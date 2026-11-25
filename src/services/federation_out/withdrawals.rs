//! The withdrawal the network is owed when a row goes away: the `Delete` of
//! a tombstoned note under the deterministic delete id, or the `Undo` of the
//! `Announce` a tombstoned reblog row was — never both, and never to an
//! audience wider than the one that heard the fact.
//!
//! The boost document this arm withdraws is [`super::boosts`]'; the note
//! machinery a plain post's `Delete` mirrors is [`super::notes`]'; the rows,
//! the addressing rule and the publisher hand are the root's.

use tracing::warn;
use uuid::Uuid;

use super::audience::{Kind, queue_with_audience};
use super::boosts::reblog_activity;
use super::notes::federates_as_note;
use super::{load_user, queue_activity, status_action};
use crate::{
    federation::note,
    models::{
        activitypub_actor,
        status::{self, Status, Visibility},
        user::followers_address_of,
    },
    state::AppState,
};

/// Take the boost down when its row is deleted, or the post itself.
///
/// The two arms answer the two kinds of row: a deleted REBLOG withdraws
/// itself with the `Undo` of the `Announce` it was (never a `Delete` — the
/// original it named stands), and a deleted plain status is taken off the
/// network with the `Delete` of its note, whose object is the `Tombstone` the
/// object route serves at that uri from here on.
///
/// One call announces the one row the delete service hands over: the reblogs
/// that `status::soft_delete_with_reblogs` tombstones along with a deleted
/// original go down unannounced — a boost is withdrawn by the delete of the
/// boost, and a delete of the post it boosted is not that withdrawal.
pub async fn after_status_deleted(state: &AppState, deleted: &Status) {
    if !state.config.federation_enabled.get() {
        return;
    }
    match deleted.reblog_of_id {
        Some(original_id) => boost_withdrawal(state, deleted, original_id).await,
        None => note_withdrawal(state, deleted).await,
    }
}

/// The `Undo` of the `Announce` a deleted reblog row was.
///
/// A `local`-visibility boost never went out, so nothing is withdrawn, and
/// the rest of the refusals are [`status_action`]'s: an actor that is gone
/// or not ours, an original that has died in the meantime.
async fn boost_withdrawal(state: &AppState, deleted: &Status, original_id: Uuid) {
    if deleted.visibility == Visibility::Local {
        return; // a local-only boost stays local
    }
    let Some(action) = status_action(state, deleted.user_id, original_id).await else {
        return;
    };
    let Some(document) = reblog_activity(state, deleted, &action, true) else {
        return;
    };
    queue_activity(state, &action.actor, &document, "boost withdrawal").await;
}

/// The `Delete` of a deleted plain status: the reserved object id as a
/// `Tombstone`, under the deterministic
/// `{BASE_URL}/activities/delete-{status id}` id and the addressing the
/// status itself travelled with.
///
/// The audience is the part that cannot be re-read: the live row is a
/// tombstone by the time this runs ([`note::load_note_context`] answers one
/// with a miss), so the two lines come from [`note::audience_lines`] run over
/// the pre-delete row the delete service holds and the mention links still
/// attached to it — the very rule that wrote the note's own `to` and `cc`,
/// which is what makes the withdrawal reach exactly the inboxes that hold
/// the post and no others. A `direct` post's `Delete` therefore goes to its
/// mentionees alone; a public one's to the author's remote followers.
///
/// The author's `is_active` flag is deliberately not a gate here, unlike
/// [`note_publisher`]'s: a withdrawal is not a post going out but a copy
/// coming down, and a peer that still holds the note of a since-deactivated
/// account has to learn it is gone.
///
/// [`note_publisher`]: super::notes::note_publisher
async fn note_withdrawal(state: &AppState, deleted: &Status) {
    if !federates_as_note(deleted) {
        return;
    }
    let Some(author) = load_user(state, deleted.user_id).await else {
        return;
    };
    if !author.local.get() {
        return; // a mirror's row belongs to its own instance's history
    }
    let mentions = match note::load_mentions(&state.db, deleted).await {
        Ok(mentions) => mentions,
        Err(err) => {
            warn!(
                %err,
                status = %deleted.id,
                "note withdrawal could not read the status' audience"
            );
            return;
        }
    };
    let followers_url = match activitypub_actor::get(&state.db, author.id).await {
        Ok(row) => row.and_then(|row| row.followers_url),
        Err(err) => {
            warn!(
                %err,
                status = %deleted.id,
                "note withdrawal could not read the author's followers collection"
            );
            return;
        }
    };
    let followers = followers_address_of(&author.ap_id, followers_url.as_deref());
    let (to, cc) = note::audience_lines(deleted.visibility, &followers, &mentions);
    let row = withdrawn_row(state, deleted).await;
    let document = note::delete_activity_json(&note::NoteDeletion {
        config: &state.config,
        status: &row,
        actor_ap_id: &author.ap_id,
        to: &to,
        cc: &cc,
    });
    queue_with_audience(state, &author, deleted.id, &document, Kind::Delete).await;
}

/// The row as the delete left it, for the `Tombstone` to report: the
/// database's own, whose `deleted_at` is the instant the withdrawal names
/// and whose document is the one `/objects/{uuid}` answers with beside its
/// `410`. The snapshot the caller holds stands in for it when the row cannot
/// be re-read at all — a purge between the delete and this hook — leaving
/// [`note::tombstone_json`] to report that row's last write instead.
async fn withdrawn_row(state: &AppState, deleted: &Status) -> Status {
    match status::get_by_id_including_deleted(&state.db, deleted.id).await {
        Ok(Some(row)) => row,
        Ok(None) => deleted.clone(),
        Err(err) => {
            warn!(
                %err,
                status = %deleted.id,
                "note withdrawal could not re-read the tombstoned row"
            );
            deleted.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        federation::vocab::AS_PUBLIC,
        models::status,
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, local, local_post, local_status,
            publish_jobs, queued_activity, remote, remote_status, run_publishes,
            the_single_activity,
        },
        util::time::mastodon_time,
    };

    /// Deleting a local reblog row sends the `Undo` of its `Announce` —
    /// never a `Delete` — and a deleted plain status answers the other way
    /// round: the `Delete` of its note, and no `Undo` beside it.
    #[tokio::test]
    async fn deleting_a_reblog_row_sends_the_undo_instead_of_a_delete() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "boost-doomed", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let doomed = local_status(&pool, &alice, "plain post").await;
        let state = app_state(&pool);

        after_status_deleted(&state, &reblog).await;
        after_status_deleted(&state, &doomed).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(
            jobs.len(),
            2,
            "the boost withdraws itself and the plain status is deleted"
        );
        let undo = queued_activity(&jobs, "Undo");
        assert_eq!(undo["object"]["type"], "Announce");
        assert_eq!(
            undo["object"]["id"], reblog.uri,
            "a boost is never a Delete"
        );
        let delete = queued_activity(&jobs, "Delete");
        assert_eq!(
            delete["object"]["type"], "Tombstone",
            "and a plain status is never an Undo"
        );
        assert_eq!(delete["object"]["id"], doomed.uri);
    }

    /// Deleting a post through the shared deletion service queues the
    /// `Delete` whose object is the status' `Tombstone`, under the
    /// deterministic id the ticket pins and the addressing the post
    /// travelled with — the withdrawal reaches exactly the hosts that hold
    /// a copy, and a retried delete names the same activity.
    #[tokio::test]
    async fn a_delete_publishes_the_delete_with_its_tombstone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let doomed = local_post(&pool, &alice, "here today", Visibility::Public).await;
        let state = app_state(&pool);

        let withdrawn = crate::services::delete_status::delete_status(&state, &doomed)
            .await
            .expect("the deletion service runs");
        assert!(withdrawn, "the row was live and went");

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Delete");
        assert_eq!(
            activity["id"],
            format!("{TEST_BASE_URL}/activities/delete-{}", doomed.id),
            "the deterministic withdrawal id"
        );
        assert_eq!(
            activity["actor"], alice.ap_id,
            "the author takes the post down"
        );
        assert_eq!(
            activity["to"],
            json!([AS_PUBLIC]),
            "the addressing the post carried"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "both of its lines"
        );
        assert_eq!(
            activity["object"]["id"], doomed.uri,
            "the reserved object id"
        );
        assert_eq!(activity["object"]["type"], "Tombstone");
        assert_eq!(activity["object"]["formerType"], "Note");
        let gone = status::get_by_id_including_deleted(&pool, doomed.id)
            .await
            .expect("the tombstone reads")
            .expect("the row is still stored, tombstoned");
        assert_eq!(
            activity["object"]["deleted"],
            mastodon_time(&gone.deleted_at.expect("the delete stamped its instant")),
            "the withdrawal reports the delete's own moment — the same one the \
             `/objects/{{uuid}}` route answers beside its 410"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the withdrawal goes exactly where the post went"
        );
    }
}
