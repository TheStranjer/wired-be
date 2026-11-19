//! The `Like` arm of the interaction hooks: the favourite row's `Like` and
//! the `Undo` that takes it back, delivered to the status author's inbox
//! and — for a public status — to the actor's remote followers.
//!
//! The three rows every interaction document is built from, the addressing
//! they share and the publisher hand that carries the document all stand in
//! the module root; the reaction twin is [`super::reactions`] and the boost
//! twin [`super::boosts`].

use super::{PUBLIC, followers_address, queue_activity, status_action, withdrawal};
use crate::{
    federation::activities::{ObjectActivityContext, like_json},
    models::favourite::Favourite,
    state::AppState,
};

/// Announce the favourite or unfavourite the caller just performed.
///
/// The document is the `Like` behind the row (or the `Undo` wrapping it),
/// delivered to the status author's inbox and — for a public status — to
/// the actor's remote followers, the pair Pleroma's
/// `CommonAPI.favorite`/`unfavorite` send.
/// The like's activity id is the row's stored `ap_id` — the id the
/// favourite write minted (decision 6) — and the unfavourite's `Undo`
/// carries a fresh one while embedding that same id. A LOCAL status behind
/// the like changes the fan-out, not the document: the publisher resolves
/// the local author to no delivery target, so a local-only audience
/// queues nothing and the remote followers' inboxes stand as the only
/// recipients. A `local`-visibility status federates nothing at all.
///
/// A row with no `ap_id` (a mirrored remote like, which this instance does
/// not own), a gone status or author, and a non-local actor publish
/// nothing.
pub async fn after_favourite_changed(state: &AppState, favourite: &Favourite, favouriting: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(like_id) = favourite.ap_id.as_deref() else {
        return; // an id-less row has no document to stand behind
    };
    let Some(action) = status_action(state, favourite.user_id, favourite.status_id).await else {
        return;
    };
    let followers = followers_address(&action.actor);
    let to = [action.author.ap_id.as_str()];
    let cc: Vec<&str> = if action.public {
        vec![followers.as_str(), PUBLIC]
    } else {
        Vec::new()
    };
    let like = like_json(&ObjectActivityContext {
        config: &state.config,
        id: like_id,
        actor_ap_id: &action.actor.ap_id,
        object_uri: &action.status.uri,
        to: &to,
        cc: &cc,
        published: &favourite.inserted_at,
    });
    let document = if favouriting {
        like
    } else {
        withdrawal(state, &action.actor.ap_id, &to, &cc, &like)
    };
    queue_activity(state, &action.actor, &document, "favourite").await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::{
            favourite,
            status::Visibility,
            user::{NewUser, insert, local_ap_id},
        },
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, job_count, local, local_status, remote,
            remote_status, run_publishes, the_single_activity,
        },
        util::time::mastodon_time,
    };

    /// Favouriting a public REMOTE status publishes the `Like` under the
    /// row's stored id, `to` the author and `cc` the followers collection
    /// and the public address — and the fan-out lands in the author's
    /// personal inbox and the followers' instance inbox, nowhere else.
    #[tokio::test]
    async fn favouriting_a_remote_status_publishes_a_like() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let liked = remote_status(&pool, &bob, "post-1", Visibility::Public).await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Like");
        assert_eq!(
            activity["id"],
            row.ap_id.expect("a local favourite carries its id"),
            "the document id is the row's stored ap_id, not a re-mint"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(activity["object"], liked.uri, "object is the status uri");
        assert_eq!(
            activity["to"].as_array().expect("to is a line").len(),
            1,
            "the author is the only primary addressee"
        );
        assert_eq!(activity["to"][0], bob.ap_id);
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), PUBLIC]),
            "the followers collection and the public address ride cc \
             because the status is public"
        );
        assert_eq!(activity["published"], mastodon_time(&row.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "the followers' shared inbox (once) and the author's personal inbox"
        );
    }

    /// Unfavouriting publishes the `Undo` that wraps the same `Like`: a
    /// fresh own-instance id for the wrapper, the stored id inside, and
    /// the like's own addressing repeated so the same inboxes hear it.
    #[tokio::test]
    async fn unfavouriting_publishes_the_undo_of_the_stored_like() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let liked = remote_status(&pool, &bob, "post-undo", Visibility::Public).await;
        let created = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let stored_id = created.ap_id.expect("a local favourite carries its id");
        let removed = favourite::unfavourite(&pool, alice.id, liked.id)
            .await
            .expect("the withdrawal runs")
            .expect("the like went");
        let state = app_state(&pool);

        after_favourite_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["actor"], alice.ap_id);
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "the undo carries this instance's own activity shape: {id}"
        );
        assert_ne!(id, stored_id, "and it is a fresh id, not the like's");
        assert_eq!(activity["object"]["type"], "Like");
        assert_eq!(
            activity["object"]["id"], stored_id,
            "the embedded like keeps the row's stored id"
        );
        assert_eq!(activity["object"]["object"], liked.uri);
        assert_eq!(
            activity["to"],
            json!([bob.ap_id]),
            "the undo repeats the like's to"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), PUBLIC]),
            "and its cc"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://remote.example/inbox",
                "https://remote.example/users/bob/inbox",
            ],
            "exactly the inboxes that heard the like hear the withdrawal"
        );
    }

    /// A like of a NOT-public status addresses the author alone: no
    /// followers collection, no public address, and the fan-out delivers
    /// to the author's inbox only.
    #[tokio::test]
    async fn a_like_of_a_non_public_status_addresses_the_author_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let liked = remote_status(&pool, &bob, "post-unlisted", Visibility::Unlisted).await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["to"], json!([bob.ap_id]));
        assert_eq!(
            activity["cc"],
            json!([]),
            "an unlisted post's like stays with the author"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the followers hear nothing about it"
        );
    }

    /// A like of a LOCAL public status still federates: the author is not
    /// a delivery target, so the remote followers' inboxes stand as the
    /// only recipients of the very same document.
    #[tokio::test]
    async fn a_like_of_a_local_public_status_reaches_the_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        let liked = local_status(&pool, &carol, "home soil").await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Like");
        assert_eq!(
            activity["to"],
            json!([carol.ap_id]),
            "the local author is addressed"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), PUBLIC]),
            "and the public fan-out line rides along"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "only the remote follower's instance is a delivery target"
        );
    }

    /// An interaction whose whole audience is local queues its publish job
    /// and expands it into nothing: a local author, local followers, and
    /// this instance's own origin are never delivery targets.
    #[tokio::test]
    async fn a_local_only_audience_yields_zero_deliveries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        accepted_follow(&pool, &bob, &alice).await;
        let liked = local_status(&pool, &bob, "between neighbours").await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;
        run_publishes(&state, &pool).await;

        assert!(
            delivered_inboxes(&pool).await.is_empty(),
            "the fan-out runs and names nothing"
        );
    }

    /// A favourite row with no stored `ap_id` — a mirrored remote like,
    /// which this instance does not own — has no document to announce:
    /// silence, as the block's twin rule does.
    #[tokio::test]
    async fn a_like_without_a_stored_id_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let liked = remote_status(&pool, &bob, "post-idless", Visibility::Public).await;
        let mut row = favourite::favourite(&pool, TEST_BASE_URL, alice.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        row.ap_id = None;
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }

    /// An actor row that carries no `followers_url` still gets a whole
    /// document: the built `{ap_id}/followers` address rides the `cc`
    /// line, and the author's own inbox stands as the delivery target.
    #[tokio::test]
    async fn an_actor_without_a_stored_followers_url_names_the_built_address() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bare = insert(
            &pool,
            &NewUser::local("bare", &local_ap_id(TEST_BASE_URL, "bare")),
        )
        .await
        .expect("the bare local fixture inserts");
        let bob = remote(&pool, "bob").await;
        let liked = remote_status(&pool, &bob, "post-bare", Visibility::Public).await;
        let row = favourite::favourite(&pool, TEST_BASE_URL, bare.id, liked.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let state = app_state(&pool);

        after_favourite_changed(&state, &row, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", bare.ap_id), PUBLIC]),
            "the built address stands in for the missing column"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the author is reached; the unexpandable collection names nobody"
        );
    }
}
