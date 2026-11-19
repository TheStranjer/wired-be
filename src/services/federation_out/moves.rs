//! The pair that announces an account's move to another actor: the `Move`
//! to the origin's followers collection, so the REMOTE followers learn where
//! their follow went, and the profile `Update` of the re-read row, so the
//! document peers hold grows its `movedTo`.
//!
//! The second half is [`super::profile`]'s hook, run here off the freshest
//! row; the followers address and the publisher hand are the root's.

use super::after_profile_updated;
use super::{followers_address, load_user, queue_activity};
use crate::{
    federation::activities::{TargetedActivityContext, move_json},
    models::user::User,
    state::AppState,
    util::id::new_id,
};

/// Announce the account `move_account` just moved onto `new`.
///
/// Two documents tell a move, both published as the origin:
///
/// - the `Move` — [`move_json`] with the origin's own actor id as both
///   `actor` and `object` and the destination's as `target`, `to` the
///   origin's followers collection. The endpoint transferred the LOCAL
///   followers before this hook runs; the remote ones keep following the
///   origin until their own instances act on this activity, so the
///   publisher's follower expansion is exactly the audience Pleroma's
///   `ActivityPub.move` delivers to. Its id is a fresh
///   `{BASE_URL}/activities/{uuid}` (decision 6);
/// - a profile `Update` — [`after_profile_updated`] run off the re-read
///   row, so the actor document it embeds carries the
///   `movedTo` the account just acquired (and its `alsoKnownAs` aliases).
///   The per-account debounce key of that hook is shared here by design:
///   a move straight after a burst of profile edits is one announcement
///   of the freshest document.
///
/// The `old` row is the pre-move snapshot the endpoint holds; the hook
/// re-reads it, and a row that vanished in between announces nothing.
pub async fn after_account_moved(state: &AppState, old: &User, new: &User) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !old.local.get() {
        return; // only our own actors announce a move
    }
    let Some(moved) = load_user(state, old.id).await else {
        return; // the account is gone; there is nothing left to tell
    };
    let followers = followers_address(&moved);
    let now = chrono::Utc::now();
    let to = [followers.as_str()];
    let document = move_json(&TargetedActivityContext {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &moved.ap_id,
        object: &moved.ap_id,
        target: &new.ap_id,
        to: &to,
        cc: &[],
        published: &now,
    });
    queue_activity(state, &moved, &document, "account move").await;
    after_profile_updated(state, &moved).await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::user::{self, Flag, NewUser, insert},
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, job_count, local, publish_jobs,
            queued_activity, remote, run_publishes,
        },
    };

    /// Moving an account publishes the `Move` to the origin's followers
    /// collection and the profile `Update` of the re-read row — the actor
    /// document it embeds carries `movedTo` — and the fan-out reaches the
    /// REMOTE followers (a local one is never a delivery target).
    #[tokio::test]
    async fn a_move_publishes_the_move_and_the_moved_update() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        accepted_follow(&pool, &neighbour, &alice).await;
        let destination = "https://other.example/users/alice";
        let target = insert(
            &pool,
            &NewUser {
                nickname: "moved-to@other.example".to_owned(),
                ap_id: destination.to_owned(),
                local: Flag::new(false),
                domain: Some("other.example".to_owned()),
                inbox: Some(format!("{destination}/inbox")),
                ..NewUser::default()
            },
        )
        .await
        .expect("the destination fixture inserts");
        user::mark_moved(&pool, alice.id, &target.ap_id)
            .await
            .expect("the move marks in");
        let state = app_state(&pool);

        after_account_moved(&state, &alice, &target).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 2, "the Move and the profile Update");

        let activity = queued_activity(&jobs, "Move");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(
            activity["object"], alice.ap_id,
            "the object is the origin's own id"
        );
        assert_eq!(activity["target"], destination);
        assert_eq!(
            activity["to"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the followers collection is the only address"
        );
        assert_eq!(activity["cc"], json!([]));
        let id = activity["id"].as_str().expect("the move has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "a fresh own-instance activity id: {id}"
        );

        let update = queued_activity(&jobs, "Update");
        assert_eq!(update["actor"], alice.ap_id);
        assert_eq!(update["object"]["id"], alice.ap_id);
        assert_eq!(
            update["object"]["movedTo"], destination,
            "the re-read row carries the move into the actor document"
        );
        assert!(
            jobs.iter()
                .any(|(_, key)| key.as_deref() == Some(&*format!("profile-update:{}", alice.id))),
            "the Update rides the per-account debounce key"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"; 2],
            "each activity reaches the remote follower's instance once, \
             and the local follower not at all"
        );
    }

    /// A mirrored account's move is its own instance's to announce.
    #[tokio::test]
    async fn a_mirror_moving_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        let state = app_state(&pool);

        after_account_moved(&state, &bob, &carol).await;

        assert_eq!(job_count(&pool).await, 0, "a mirror announces nothing");
    }
}
