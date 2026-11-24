//! The `Add` / `Remove` announcements of a local account's `featured`
//! collection: a post pinned or unpinned.
//!
//! They travel to the public with the author's followers collection in
//! `cc`; the followers address, the row readers and the publisher hand are
//! the root's. List membership is private: it is never announced.

use super::{load_status, load_user, queue_activity};
use crate::{
    federation::{
        activities::{TargetedActivityContext, add_json, remove_json},
        vocab::AS_PUBLIC,
    },
    models::{
        pinned_status::PinnedStatus,
        user::{User, featured, followers_address},
    },
    state::AppState,
    util::id::new_id,
};

/// Announce the pin the caller just stored or withdrew.
///
/// The document is Pleroma's `Builder.pin` / `Builder.unpin`: [`add_json`]
/// or [`remove_json`] with the pinned status' uri as `object` and the
/// author's featured collection as `target`, `to` the public and the
/// author's followers collection in `cc` — the fan-out that reaches the
/// REMOTE followers, whose instances mirror the profile the feature rides.
/// A local actor mirrors nothing and announces nothing; a gone author or a
/// gone status has no document to build. (Only `public` and `unlisted`
/// posts can be pinned at all — the endpoint's gate, not checked here.)
pub async fn after_pin_changed(state: &AppState, pinned: &PinnedStatus, pinning: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(actor) = load_user(state, pinned.user_id).await else {
        return; // an author whose row is gone has no collection to move
    };
    if !actor.local.get() {
        return; // a mirrored pin belongs to its own instance
    }
    let Some(pinned_status) = load_status(state, pinned.status_id).await else {
        return; // a gone status leaves no feature to announce
    };
    let followers = followers_address(&actor);
    let featured_collection = featured_address(&actor);
    let to = [AS_PUBLIC];
    let cc = [followers.as_str()];
    let ctx = TargetedActivityContext {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &actor.ap_id,
        object: &pinned_status.uri,
        target: &featured_collection,
        to: &to,
        cc: &cc,
        published: &pinned.inserted_at,
    };
    let document = if pinning {
        add_json(&ctx)
    } else {
        remove_json(&ctx)
    };
    queue_activity(state, &actor, &document, "pin").await;
}

/// The address naming the actor's featured (pinned) collection: the stored
/// `featured_url` — the very url a peer's `Add`/`Remove` handler matches
/// the `target` against — with the [`featured`] construction behind a row
/// that carries none, the way [`followers_address`] treats its column.
///
/// [`featured`]: crate::models::user::featured
fn featured_address(actor: &User) -> String {
    actor
        .featured_url
        .clone()
        .unwrap_or_else(|| featured(&actor.ap_id))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::{
            pinned_status,
            user::{NewUser, featured, insert, local_ap_id},
        },
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, job_count, local, local_status, remote,
            run_publishes, the_single_activity,
        },
        util::time::mastodon_time,
    };

    /// A registered-looking local actor with a featured collection too:
    /// exactly the row `register` writes, the pin hook's publisher.
    async fn pinner(pool: &PgPool, nickname: &str) -> User {
        let ap_id = local_ap_id(TEST_BASE_URL, nickname);
        insert(
            pool,
            &NewUser {
                private_key: Some("PEM".to_owned()),
                public_key: Some("PUB".to_owned()),
                inbox: Some(format!("{ap_id}/inbox")),
                followers_url: Some(format!("{ap_id}/followers")),
                featured_url: Some(featured(&ap_id)),
                ..NewUser::local(nickname, &ap_id)
            },
        )
        .await
        .expect("the pinning fixture inserts")
    }

    /// Pinning publishes the `Add` that features the status in the
    /// author's collection, `to` the public with the followers in `cc` —
    /// and the fan-out reaches the REMOTE followers only.
    #[tokio::test]
    async fn a_pin_publishes_the_add_to_the_featured_collection() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = pinner(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        accepted_follow(&pool, &neighbour, &alice).await;
        let featured_post = local_status(&pool, &alice, "look at me").await;
        let row = pinned_status::pin(&pool, alice.id, featured_post.id)
            .await
            .expect("the pin writes")
            .expect("the pair pinned");
        let state = app_state(&pool);

        after_pin_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Add");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], featured_post.uri);
        assert_eq!(
            activity["target"],
            alice
                .featured_url
                .clone()
                .expect("the fixture has a collection"),
            "the target is the actor's own featured collection"
        );
        assert_eq!(activity["to"], json!([AS_PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the followers ride cc — the public address itself names nobody"
        );
        let id = activity["id"].as_str().expect("the add has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "a fresh own-instance activity id: {id}"
        );
        assert_eq!(activity["published"], mastodon_time(&row.inserted_at));
        let document = crate::federation::actor::actor_json(&alice, &state.config);
        assert_eq!(
            activity["target"], document["featured"],
            "the target is the very collection the actor document advertises"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the remote follower's instance only; the local one heard nothing"
        );
    }

    /// Unpinning publishes the `Remove` on the same skeleton: the same
    /// object, the same target, the same audience.
    #[tokio::test]
    async fn an_unpin_publishes_the_remove() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = pinner(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        let featured_post = local_status(&pool, &alice, "was featured").await;
        let _created = pinned_status::pin(&pool, alice.id, featured_post.id)
            .await
            .expect("the pin writes");
        let removed = pinned_status::unpin(&pool, alice.id, featured_post.id)
            .await
            .expect("the withdrawal runs")
            .expect("the pin went");
        let state = app_state(&pool);

        after_pin_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Remove");
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], featured_post.uri);
        assert_eq!(
            activity["target"],
            alice.featured_url.expect("the fixture has a collection")
        );
        assert_eq!(activity["to"], json!([AS_PUBLIC]));
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id)])
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the withdrawal travels to the remote followers alone"
        );
    }

    /// An actor row with no `featured_url` still gets a whole document:
    /// the built `{ap_id}/collections/featured` address rides the `target`.
    #[tokio::test]
    async fn a_pin_without_a_stored_featured_url_names_the_built_address() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let featured_post = local_status(&pool, &alice, "feature me").await;
        let row = pinned_status::pin(&pool, alice.id, featured_post.id)
            .await
            .expect("the pin writes")
            .expect("the pair pinned");
        let state = app_state(&pool);

        after_pin_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["target"],
            featured(&alice.ap_id),
            "the built address stands in for the missing column"
        );
    }

    /// A mirrored account's pin and a pin of a status that has since died
    /// announce nothing.
    #[tokio::test]
    async fn pins_ours_to_announce_answer_with_silence() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = pinner(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mirrored_pin = pinned_status::pin(
            &pool,
            bob.id,
            local_status(&pool, &alice, "alice speaks").await.id,
        )
        .await
        .expect("the mirror pin writes")
        .expect("the pair pinned");
        let doomed_post = local_status(&pool, &alice, "about to go").await;
        let orphaned_pin = pinned_status::pin(&pool, alice.id, doomed_post.id)
            .await
            .expect("the pin writes")
            .expect("the pair pinned");
        let _erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(doomed_post.id)
            .execute(&pool)
            .await
            .expect("the fixture erases the status");
        let state = app_state(&pool);

        after_pin_changed(&state, &mirrored_pin, true).await;
        after_pin_changed(&state, &orphaned_pin, true).await;
        after_pin_changed(&state, &mirrored_pin, false).await;

        assert_eq!(job_count(&pool).await, 0, "neither row is ours to tell");
    }
}
