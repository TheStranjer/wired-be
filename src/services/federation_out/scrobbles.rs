//! The `Listen` arm of the hooks: the activity a fresh scrobble federates
//! as, delivered to the listener's remote followers.

use tracing::warn;

use super::{load_user, queue_activity};
use crate::{
    federation::activities::listen_json,
    models::{activitypub_actor, scrobble::Scrobble, status::Visibility},
    state::AppState,
};

/// Announce the scrobble the caller just posted: the `Listen` activity
/// wrapping its `Audio` object.
///
/// The document is [`listen_json`] — the pair the `/activities/{uuid}` and
/// `/objects/{uuid}` routes serve — published as the listener, whose
/// followers collection the document's own `to` and `cc` name, so the
/// publisher expands it into the remote followers' inboxes.
///
/// Silence is the answer of a `direct` scrobble (it addresses nobody), of
/// the status-only `local` visibility, and of a listener that is gone, not
/// one of our own, or switched off.
pub async fn after_scrobble_created(state: &AppState, scrobble: &Scrobble) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if matches!(scrobble.visibility, Visibility::Direct | Visibility::Local) {
        return;
    }
    let Some(listener) = load_user(state, scrobble.user_id).await else {
        return;
    };
    if !listener.is_local_active() {
        return;
    }
    let followers = match activitypub_actor::account_followers(&state.db, &listener).await {
        Ok(address) => address,
        Err(err) => {
            warn!(%err, listener = %listener.ap_id, "could not read the listener's actor row");
            return;
        }
    };
    let document = listen_json(scrobble, &listener, &followers, &state.config);
    queue_activity(state, &listener, &document, "scrobble").await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        federation::vocab::AS_PUBLIC,
        models::{
            scrobble::{self, NewScrobble},
            user::User,
        },
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, job_count, local, remote, run_publishes,
            state_with, the_single_activity, unfederated,
        },
    };

    /// Stores a scrobble of `visibility` for `listener`.
    async fn scrobbled(pool: &PgPool, listener: &User, visibility: Visibility) -> Scrobble {
        scrobble::insert(
            pool,
            TEST_BASE_URL,
            listener.id,
            &NewScrobble {
                title: "Wired Song".to_owned(),
                artist: Some("The Lain".to_owned()),
                visibility,
                ..NewScrobble::default()
            },
        )
        .await
        .expect("the scrobble fixture inserts")
    }

    /// A public scrobble queues one `Listen` with its `Audio` embedded, and
    /// the fan-out reaches the remote follower's shared inbox.
    #[tokio::test]
    async fn a_public_scrobble_publishes_a_listen_to_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let row = scrobbled(&pool, &alice, Visibility::Public).await;
        let state = app_state(&pool);

        after_scrobble_created(&state, &row).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Listen");
        assert_eq!(
            activity["id"],
            row.ap_id.clone().expect("a local scrobble has its id"),
            "the document id is the row's stored ap_id"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["to"], json!([AS_PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)])
        );
        assert_eq!(activity["object"]["type"], "Audio");
        assert_eq!(
            activity["object"]["id"],
            format!("{TEST_BASE_URL}/objects/{}", row.id)
        );
        assert_eq!(activity["object"]["title"], "Wired Song");
        assert_eq!(activity["object"]["artist"], "The Lain");

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "one delivery, at the follower's shared inbox"
        );
    }

    /// A `direct` scrobble addresses nobody, so nothing is queued at all.
    #[tokio::test]
    async fn a_direct_scrobble_queues_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let row = scrobbled(&pool, &alice, Visibility::Direct).await;
        let state = app_state(&pool);

        after_scrobble_created(&state, &row).await;
        run_publishes(&state, &pool).await;

        assert_eq!(job_count(&pool).await, 0, "no publish, no delivery");
    }

    /// A `private` scrobble still reaches the followers it is addressed to.
    #[tokio::test]
    async fn a_private_scrobble_reaches_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let row = scrobbled(&pool, &alice, Visibility::Private).await;
        let state = app_state(&pool);

        after_scrobble_created(&state, &row).await;
        run_publishes(&state, &pool).await;

        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "followers-only is still delivered to followers"
        );
    }

    /// With federation off nothing is queued.
    #[tokio::test]
    async fn a_disabled_federation_queues_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let row = scrobbled(&pool, &alice, Visibility::Public).await;

        after_scrobble_created(&state_with(&pool, unfederated()), &row).await;

        assert_eq!(job_count(&pool).await, 0, "federation is off");
    }

    /// A switched-off listener's scrobble stays on this instance.
    #[tokio::test]
    async fn a_deactivated_listener_queues_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let row = scrobbled(&pool, &alice, Visibility::Public).await;
        let switched = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the deactivation fixture writes")
            .rows_affected();
        assert_eq!(switched, 1, "the listener is there to switch off");

        after_scrobble_created(&app_state(&pool), &row).await;

        assert_eq!(job_count(&pool).await, 0, "a dead account is silent");
    }
}
