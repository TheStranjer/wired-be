//! The actor `Update` an account owes its peers when it is suspended,
//! deactivated, unsuspended or re-enabled.

use tracing::debug;

use super::profile::{known_inboxes, publish_to_known, update_document};
use crate::{
    error::AppResult,
    federation::actor::profile_emojis,
    models::{
        activitypub_actor,
        user::{User, get_by_id},
    },
    state::AppState,
};

/// Announce the activation state `account` is in now, as an actor `Update`.
///
/// Mastodon's `SuspendAccountService` sends an `Update` of the actor whose
/// document says `"suspended": true` with the profile stripped, and
/// unsuspending sends another `Update` without the flag. This does both with
/// one function, because the document speaks for itself: it is rendered from
/// the row as it stands when this runs ([`actor_json`] emits the minimal
/// `suspended` document for an inactive account and the full profile for an
/// active one), so the adapter runs it for `Suspended`, `Deactivated`,
/// `Unsuspended` and `Reactivated` alike. The `account` it is handed may be
/// the row read before the write, so the current row is read again here.
///
/// The recipients are the set [`after_account_deleted`] uses: the remote
/// followers and followees, the accepted relays, then every other remote
/// inbox that may hold the account's posts.
///
/// Decision, made here and nowhere else: suspension does **not** also
/// `Reject` the remote follows, as Mastodon's `reject_remote_follows!` does.
/// A `Reject` makes the peer drop the follow for good, and re-enabling this
/// account could not restore it, whereas here `enable` and `unsuspend`
/// reverse a suspension completely; the `suspended` document is what tells
/// peers to hide the account in the meantime.
///
/// Its one non-test caller is `ActivityPub`'s adapter, on the four
/// activation events.
///
/// [`actor_json`]: crate::federation::actor::actor_json
/// [`after_account_deleted`]: super::after_account_deleted
///
/// # Errors
///
/// Any failure to read the account, its recipients or its emoji, or to queue
/// a publish. Federation disabled, a remote account and an account nobody
/// knew all answer `Ok(())`.
pub async fn after_account_activation_changed(state: &AppState, account: &User) -> AppResult<()> {
    if !state.config.federation_enabled.get() || !account.local.get() {
        return Ok(()); // only our own actors announce themselves
    }
    let current = get_by_id(&state.db, account.id)
        .await?
        .unwrap_or_else(|| account.clone());
    let inboxes = known_inboxes(state, &current).await?;
    if inboxes.peers.is_empty() && inboxes.long_tail.is_empty() {
        return Ok(()); // nobody on another instance ever knew this account
    }
    let row = activitypub_actor::get(&state.db, current.id).await?;
    let followers = activitypub_actor::followers_address(row.as_ref(), &current.ap_id);
    let emojis = profile_emojis(&state.db, &current).await?;
    let document = update_document(state, &current, &followers, row.as_ref(), &emojis);
    publish_to_known(state, &current, &document, &inboxes, "activation Update").await?;
    debug!(actor = %current.ap_id, active = current.is_active.get(), "queued activation Update");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::user::set_activation,
        services::federation_out::test_support::{
            accepted_follow, app_state, job_count, local, publish_jobs, remote, state_with,
            unfederated,
        },
    };

    /// Suspending queues an `Update` of the minimal `suspended` document for
    /// the follower, even though the caller still holds the pre-write row;
    /// re-enabling queues a second one with the profile and no flag.
    #[tokio::test]
    async fn suspension_and_its_reversal_each_publish_an_update() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        let state = app_state(&pool);

        set_activation(&pool, alice.id, false)
            .await
            .expect("the suspension writes");
        after_account_activation_changed(&state, &alice)
            .await
            .expect("the suspension federates");
        set_activation(&pool, alice.id, true)
            .await
            .expect("the enable writes");
        after_account_activation_changed(&state, &alice)
            .await
            .expect("the enable federates");

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 2, "one Update per change, never debounced");
        let (suspended, unsuspended) = (&jobs[0].0, &jobs[1].0);
        assert!(jobs[0].1.is_none() && jobs[1].1.is_none(), "no dedup key");
        for args in [suspended, unsuspended] {
            assert_eq!(args["activity"]["type"], "Update", "an Update");
            assert_eq!(
                args["activity"]["object"]["id"], alice.ap_id,
                "of the actor"
            );
            assert_eq!(
                args["extra_inboxes"],
                serde_json::json!(["https://remote.example/inbox"]),
                "to the follower's instance"
            );
        }
        let stripped = &suspended["activity"]["object"];
        assert_eq!(stripped["suspended"], true, "the flag");
        assert!(stripped.get("name").is_none(), "the profile is stripped");
        assert!(stripped["publicKey"].is_object(), "the key stays");
        let restored = &unsuspended["activity"]["object"];
        assert!(restored.get("suspended").is_none(), "the flag is gone");
        assert!(restored.get("name").is_some(), "the profile is back");
        assert_ne!(
            suspended["activity"]["id"], unsuspended["activity"]["id"],
            "two distinct activities"
        );
    }

    /// Nothing is queued with federation off, or for a remote mirror.
    #[tokio::test]
    async fn federation_off_and_mirrors_queue_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        accepted_follow(&pool, &bob, &alice).await;
        set_activation(&pool, alice.id, false)
            .await
            .expect("the suspension writes");

        let off = state_with(&pool, unfederated());
        after_account_activation_changed(&off, &alice)
            .await
            .expect("federation off is quiet");
        after_account_activation_changed(&app_state(&pool), &bob)
            .await
            .expect("a mirror is quiet");

        assert_eq!(job_count(&pool).await, 0, "no job in either case");
    }

    /// An account no other instance ever knew has nobody to tell.
    #[tokio::test]
    async fn an_account_nobody_knows_queues_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let lonely = local(&pool, "lonely").await;
        set_activation(&pool, lonely.id, false)
            .await
            .expect("the suspension writes");

        after_account_activation_changed(&app_state(&pool), &lonely)
            .await
            .expect("nobody to tell is quiet");

        assert_eq!(job_count(&pool).await, 0, "no job");
    }
}
