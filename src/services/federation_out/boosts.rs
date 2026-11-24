//! The `Announce` arm of the interaction hooks: the boost a reblog row *is*
//! (decision 6 — its document id is the row's stored `uri`) and the `Undo`
//! that withdraws it, addressed by the boost's own visibility.
//!
//! [`reblog_activity`] builds both, and the delete hook hands a tombstoned
//! boost back to the very same `Undo` — see [`super::withdrawals`]. The
//! shared rows, addressing and publisher hand stand in the module root; the
//! like and reaction twins are [`super::likes`] and [`super::reactions`].

use serde_json::Value;

use super::{StatusAction, queue_activity, status_action, withdrawal};
use crate::{
    federation::note::announce_activity_json,
    models::status::{Status, Visibility},
    state::AppState,
};

/// Announce the boost or unboost the caller just performed.
///
/// The document is the `Announce` the reblog row *is* (decision 6 — its
/// id is the row's stored `uri`, which is what a withdrawal's `Undo`
/// embeds and a peer's later fetch names back), or the `Undo` wrapping it.
///
/// The addressing follows the boost's own visibility as Mastodon's
/// `TagManager` does: a public one names the world in `to` and the
/// followers collection plus the boosted status' author in `cc`; an
/// unlisted one names the followers in `to` and the author plus the
/// world in `cc`; a private one the followers in `to` and the author
/// alone in `cc` — so the fan-out reaches the remote followers' and
/// the original author's inboxes, and a boost of a LOCAL status federates
/// to the followers alone (the publisher resolves the local author to
/// nothing). A `local`-visibility boost federates nothing, and neither
/// does one whose original was `local`-visibility: those facts are this
/// instance's alone.
pub async fn after_reblog_changed(state: &AppState, reblog: &Status, reblogging: bool) {
    if !state.config.federation_enabled.get() {
        return;
    }
    let Some(original_id) = reblog.reblog_of_id else {
        return; // not a reblog row: there is no Announce behind it
    };
    if reblog.visibility == Visibility::Local {
        return; // a local-only boost stays local
    }
    let Some(action) = status_action(state, reblog.user_id, original_id).await else {
        return;
    };
    let Some(document) = reblog_activity(state, reblog, &action, !reblogging) else {
        return;
    };
    queue_activity(state, &action.actor, &document, "reblog").await;
}

/// The `Announce` behind the reblog row `reblog`, in the undo arm wrapped
/// in the `Undo` that withdraws it; both travel under the addressing the
/// boost's visibility states ([`after_reblog_changed`]'s module-doc rule).
///
/// The `Announce` is [`announce_activity_json`], the one renderer the
/// object and outbox routes serve from too, so what a peer re-fetches is
/// what was delivered; the `Undo` repeats that document's own `to` and
/// `cc`. `None` for a `local` boost, which federates nothing.
pub(super) fn reblog_activity(
    state: &AppState,
    reblog: &Status,
    action: &StatusAction,
    undo: bool,
) -> Option<Value> {
    let announce = announce_activity_json(
        reblog,
        &action.actor,
        &action.status.uri,
        &action.author.ap_id,
        &state.config,
    )?;
    if !undo {
        return Some(announce);
    }
    let to = address_line(&announce, "to");
    let cc = address_line(&announce, "cc");
    Some(withdrawal(state, &action.actor.ap_id, &to, &cc, &announce))
}

/// The entries of `document`'s addressing line `name` (`to` or `cc`), in
/// order; a missing or malformed line reads as empty.
fn address_line<'a>(document: &'a Value, name: &str) -> Vec<&'a str> {
    document[name]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        federation::vocab::AS_PUBLIC,
        models::{
            emoji_reaction::{self, NewReaction},
            favourite,
            status::{self, NewStatus},
        },
        services::federation_out::{
            after_favourite_changed, after_reaction_changed, after_status_deleted,
            test_support::{
                accepted_follow, app_state, delivered_inboxes, job_count, local, local_status,
                remote, remote_status, run_publishes, the_single_activity,
            },
        },
        util::time::mastodon_time,
    };

    /// Boosting a REMOTE status publishes the `Announce` the row *is*:
    /// its stored `uri` as the id, the world in `to`, and the followers
    /// collection beside the boosted author in `cc` — the fan-out lands
    /// in the author's inbox and the followers' instance inbox.
    #[tokio::test]
    async fn boosting_a_remote_status_publishes_the_announce() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let carol = remote(&pool, "carol").await;
        accepted_follow(&pool, &carol, &alice).await;
        let original = remote_status(&pool, &bob, "boost-me", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Announce");
        assert_eq!(
            activity["id"], reblog.uri,
            "the id is the reblog row's own uri (decision 6)"
        );
        assert_eq!(activity["actor"], alice.ap_id);
        assert_eq!(
            activity["object"], original.uri,
            "object is the original uri"
        );
        assert_eq!(
            activity["to"],
            json!([AS_PUBLIC]),
            "a public boost addresses the world"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "the followers collection and the boosted author ride cc"
        );
        assert_eq!(activity["published"], mastodon_time(&reblog.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the boosted author and the remote followers share one host, hence one delivery"
        );
    }

    /// An unlisted boost names the followers collection in `to` and puts
    /// the boosted author and the public address in `cc` (Mastodon's
    /// `TagManager` shape).
    #[tokio::test]
    async fn an_unlisted_boost_addresses_the_followers_in_to() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "boost-quietly", Visibility::Public).await;
        let (reblog, wrote) = status::create_reblog(
            &pool,
            TEST_BASE_URL,
            &alice,
            &original,
            Visibility::Unlisted,
        )
        .await
        .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["to"],
            json!([format!("{}/followers", alice.ap_id)]),
            "an unlisted boost talks to the followers, not the world"
        );
        assert_eq!(
            activity["cc"],
            json!([bob.ap_id, AS_PUBLIC]),
            "the author and the public address ride cc"
        );
    }

    /// A private boost carries no public address anywhere: followers in
    /// `to`, the boosted author alone in `cc`.
    #[tokio::test]
    async fn a_private_boost_carries_no_public_address() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "boost-privately", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Private)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(
            activity["to"],
            json!([format!("{}/followers", alice.ap_id)]),
            "a private boost talks to the followers"
        );
        assert_eq!(
            activity["cc"],
            json!([bob.ap_id]),
            "only the boosted author rides cc"
        );
        for line in ["to", "cc"] {
            assert!(
                !activity[line]
                    .as_array()
                    .expect("the line is an array")
                    .contains(&json!(AS_PUBLIC)),
                "no public address in {line}"
            );
        }
    }

    /// Withdrawing an unlisted boost repeats the unlisted addressing on
    /// the `Undo`.
    #[tokio::test]
    async fn unboosting_an_unlisted_boost_repeats_the_unlisted_addressing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "unlisted-then-drop", Visibility::Public).await;
        let (_, wrote) = status::create_reblog(
            &pool,
            TEST_BASE_URL,
            &alice,
            &original,
            Visibility::Unlisted,
        )
        .await
        .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let removed = status::remove_reblog(&pool, alice.id, original.id)
            .await
            .expect("the withdrawal runs")
            .expect("the boost went");
        let state = app_state(&pool);

        after_reblog_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Undo");
        assert_eq!(
            activity["to"],
            json!([format!("{}/followers", alice.ap_id)]),
            "the unlisted to line repeats"
        );
        assert_eq!(
            activity["cc"],
            json!([bob.ap_id, AS_PUBLIC]),
            "and its cc line, public address included"
        );
    }

    /// Boosting a LOCAL status federates: the document addresses the
    /// boosted local author in `cc` (never a delivery target) and the
    /// fan-out lands in the remote followers' inboxes alone.
    #[tokio::test]
    async fn boosting_a_local_status_federates_the_announce_to_the_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let fan = remote(&pool, "fan").await;
        accepted_follow(&pool, &fan, &alice).await;
        let original = local_status(&pool, &carol, "boostable at home").await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Announce");
        assert_eq!(activity["id"], reblog.uri);
        assert_eq!(activity["object"], original.uri);
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), carol.ap_id]),
            "the local author rides cc as any boosted author would"
        );

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/inbox"],
            "the announce travels to the remote followers alone"
        );
    }

    /// Unboosting publishes the `Undo` that embeds the `Announce` by its
    /// stored uri and repeats the boost's addressing.
    #[tokio::test]
    async fn unboosting_publishes_the_undo_of_the_announce() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original = remote_status(&pool, &bob, "boost-then-drop", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let announce_uri = reblog.uri.clone();
        let removed = status::remove_reblog(&pool, alice.id, original.id)
            .await
            .expect("the withdrawal runs")
            .expect("the boost went");
        let state = app_state(&pool);

        after_reblog_changed(&state, &removed, false).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Undo");
        assert_eq!(activity["object"]["type"], "Announce");
        assert_eq!(
            activity["object"]["id"], announce_uri,
            "the embedded announce keeps the row's uri, tombstone or not"
        );
        let id = activity["id"].as_str().expect("the undo has an id");
        assert!(
            id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
            "the undo carries this instance's own activity shape: {id}"
        );
        assert_ne!(
            id, announce_uri,
            "the undo mints its OWN uuid, distinct from the boost's uri"
        );
        assert_eq!(
            activity["to"],
            json!([AS_PUBLIC]),
            "the boost's to line repeats"
        );
        assert_eq!(
            activity["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "and its cc line"
        );
    }

    /// A boost of a `local`-visibility original never federates, and
    /// neither do likes or reactions of a `local`-visibility status: the
    /// fact stays this instance's own.
    #[tokio::test]
    async fn nothing_federates_for_local_visibility_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let hidden = remote_status(&pool, &bob, "instance-only", Visibility::Local).await;
        let liked = favourite::favourite(&pool, TEST_BASE_URL, alice.id, hidden.id)
            .await
            .expect("the favourite writes")
            .expect("the pair liked");
        let new = NewReaction {
            user_id: alice.id,
            status_id: hidden.id,
            emoji: "🎉",
            url: None,
        };
        let reacted = emoji_reaction::react(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the reaction writes");
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &hidden, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_favourite_changed(&state, &liked, true).await;
        after_reaction_changed(&state, &reacted, true).await;
        after_reblog_changed(&state, &reblog, true).await;
        after_reblog_changed(&state, &reblog, false).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local-visibility post moves no queue"
        );
    }

    /// A boost whose own row is `local`-visibility never federates
    /// either — the row itself, not just its original, can hold the fact
    /// in.
    #[tokio::test]
    async fn a_local_visibility_boost_never_federates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let original =
            remote_status(&pool, &bob, "public-for-local-boost", Visibility::Public).await;
        let (reblog, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &alice, &original, Visibility::Local)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the pair boosted");
        let state = app_state(&pool);

        after_reblog_changed(&state, &reblog, true).await;
        after_reblog_changed(&state, &reblog, false).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local-only boost moves no queue"
        );
    }

    /// The mirrored rows the inbound handlers fold belong to their maker's
    /// instance: a remote actor's like, reaction, boost — and the delete
    /// of a mirror boost — publish nothing.
    #[tokio::test]
    async fn a_remote_actors_interactions_publish_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let own = local_status(&pool, &alice, "alice speaks").await;
        let mirrored_like = favourite::favourite_like(
            &pool,
            bob.id,
            own.id,
            "https://remote.example/activities/like-9",
        )
        .await
        .expect("the mirror like writes")
        .expect("the pair liked");
        let new = NewReaction {
            user_id: bob.id,
            status_id: own.id,
            emoji: "🎉",
            url: None,
        };
        let mirrored_react = emoji_reaction::react_with_ap_id(
            &pool,
            &new,
            "https://remote.example/activities/react-9",
        )
        .await
        .expect("the mirror reaction writes");
        let mirror_reblog = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                uri: Some("https://remote.example/activities/announce-9".to_owned()),
                user_id: bob.id,
                reblog_of_id: Some(own.id),
                ..NewStatus::default()
            },
        )
        .await
        .expect("the mirror boost writes");
        let state = app_state(&pool);

        after_favourite_changed(&state, &mirrored_like, true).await;
        after_reaction_changed(&state, &mirrored_react, true).await;
        after_reblog_changed(&state, &mirror_reblog, true).await;
        after_status_deleted(&state, &mirror_reblog).await;

        assert_eq!(job_count(&pool).await, 0, "a mirror announces nothing");
    }
}
