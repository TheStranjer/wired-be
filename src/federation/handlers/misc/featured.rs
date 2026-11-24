//! The `Add` and `Remove` routes: the featured-collection pins.
//! [`handle_add`] and [`handle_remove`] fold one shared body,
//! [`apply_featured_change`] — only a `target` equal to the actor's own
//! `featured_url` and a status the actor authored can be pinned. An `Add`
//! of a status this instance has not stored fetches it when it lives on
//! the actor's own host; a `Remove` only looks it up.

use futures::future::BoxFuture;
use tracing::debug;

use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        fetch_object, json,
    },
    models::{
        pinned_status,
        status::{self, Status},
        user::User,
    },
    state::AppState,
};

/// The `Add` route: pin the actor's own stored status to its featured
/// collection.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the pin
/// from being written; anything else has nothing to fold and is
/// [`Outcome::Ignored`].
pub fn handle_add(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_featured_change(state, view, signer, true).await })
}

/// The `Remove` route: take the actor's pin of its own stored status back.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the pin
/// from being removed; anything else has nothing to fold and is
/// [`Outcome::Ignored`].
pub fn handle_remove(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_featured_change(state, view, signer, false).await })
}

/// Resolves the object of an `Add` or `Remove` that is not stored.
///
/// Only an `Add` fetches, and only an object on the signer's own host: the
/// fetch is the status resolver's own (`resolve_status_as_actor`), which
/// stores nothing the signer did not author. A `Remove` has nothing to
/// take back that was never stored. `None` means there is nothing to pin.
///
/// # Errors
///
/// The resolver's [`FederationError`]: transient for a network or database
/// failure that may pass on a retry, permanent for a document it refuses.
async fn fetch_unstored(
    state: &AppState,
    uri: &str,
    signer: &User,
    pinning: bool,
) -> Result<Option<Status>, FederationError> {
    let own_host = json::http_host(uri).is_some_and(|host| {
        json::http_host(&signer.ap_id).is_some_and(|signer_host| host == signer_host)
    });
    if !pinning || !own_host {
        debug!(
            uri,
            pinning,
            own_host,
            "an Add or Remove of a status this instance does not store; ignoring"
        );
        return Ok(None);
    }
    let resolved = fetch_object::resolve_status_as_actor(state, uri, signer).await?;
    if resolved.is_none() {
        debug!(uri, "an Add of a status that resolved to nothing foldable");
    }
    Ok(resolved)
}

/// The shared body of [`handle_add`] and [`handle_remove`]: the featured
/// collection's pins, folded for the signer alone.
async fn apply_featured_change(
    state: AppState,
    view: ActivityView,
    signer: User,
    pinning: bool,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Add or Remove signed by an account this instance owns",
        ));
    }
    let featured = view
        .target
        .as_deref()
        .is_some_and(|target| signer.featured_url.as_deref() == Some(target));
    if !featured {
        debug!(
            target = view.target.as_deref().unwrap_or_default(),
            "an Add or Remove about a collection other than the actor's featured one; ignoring"
        );
        return Ok(Outcome::Ignored(
            "an Add or Remove whose target is not the actor's featured collection",
        ));
    }
    let Some(uri) = view.object.id() else {
        return Ok(Outcome::Ignored(
            "an Add or Remove whose object names no id",
        ));
    };
    let mut stored = status::get_by_uri(&state.db, uri).await.map_err(|err| {
        FederationError::transient("could not fold the remote activity's featured status", &err)
    })?;
    if stored.is_none() {
        stored = fetch_unstored(&state, uri, &signer, pinning).await?;
    }
    let Some(stored) = stored else {
        return Ok(Outcome::Ignored(
            "an Add or Remove whose object is not a stored status",
        ));
    };
    if stored.user_id != signer.id {
        return Ok(Outcome::Ignored(
            "an Add or Remove of a status its actor did not author",
        ));
    }
    if pinning {
        let _pinned = pinned_status::pin(&state.db, signer.id, stored.id)
            .await
            .map_err(|err| {
                FederationError::transient(
                    "could not fold the remote activity's featured pin",
                    &err,
                )
            })?;
    } else {
        let _unpinned = pinned_status::unpin(&state.db, signer.id, stored.id)
            .await
            .map_err(|err| {
                FederationError::transient(
                    "could not fold the remote activity's featured pin",
                    &err,
                )
            })?;
    }
    debug!(
        actor = %signer.nickname,
        status = %stored.id,
        pinning,
        "an inbound Add/Remove folded the actor's featured pin"
    );
    Ok(Outcome::Handled)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::federation::handlers::misc::test_support::*;

    /// The featured-collection route: an `Add` of the actor's own stored
    /// status pins it (restating the pin is one row), a `Remove` takes it
    /// back, and a restated `Remove` of the gone pin is the same settled
    /// fact.
    #[tokio::test]
    async fn an_add_pins_and_a_remove_unpins_the_actors_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let posted = remote_post(&pool, &alice, "featured").await;

        let first = handle_add(
            state.clone(),
            view(
                "Add",
                "add-1",
                ALICE,
                json!({"object": posted.uri.clone(), "target": FEATURED}),
            ),
            alice,
        )
        .await;
        assert_handled(&first);
        assert_eq!(
            pinned_status::list_for_user(&pool, alice_id)
                .await
                .unwrap()
                .into_iter()
                .map(|pinned| pinned.id)
                .collect::<Vec<Uuid>>(),
            vec![posted.id],
            "the pin landed on the actor's own status"
        );

        let restated = handle_add(
            state.clone(),
            view(
                "Add",
                "add-2",
                ALICE,
                json!({"object": posted.uri.clone(), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&restated);
        assert_eq!(
            pinned_status::count(&pool, alice_id).await.unwrap(),
            1,
            "and the second Add stands as the one pin it restates"
        );

        for n in ["remove-1", "remove-2"] {
            let withdrawn = handle_remove(
                state.clone(),
                view(
                    "Remove",
                    n,
                    ALICE,
                    json!({"object": posted.uri.clone(), "target": FEATURED}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await;
            assert_handled(&withdrawn);
            assert!(
                pinned_status::list_for_user(&pool, alice_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "{n} leaves nothing pinned"
            );
        }
    }

    /// The `Add`/`Remove` target arm: a `target` that is not the actor's
    /// featured collection — a different collection, none at all, or one an
    /// actor with no featured url cannot match — is ignored before anything
    /// is read.
    #[tokio::test]
    async fn an_add_or_remove_on_another_collection_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;
        let bob = local_user(&pool, "pin-bob").await;
        let bobs_post = local_post(&pool, &bob, "not alice's").await;

        let wrong_collection = handle_add(
            state.clone(),
            view(
                "Add",
                "add-3",
                ALICE,
                json!({
                    "object": bobs_post.uri.clone(),
                    "target": "https://peer.example/users/alice/collections/followers",
                }),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(wrong_collection).contains("featured collection"),
            "only the featured collection pins anything"
        );

        let no_target = handle_add(
            state.clone(),
            object_view("Add", "add-4", ALICE, json!(bobs_post.uri.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(no_target).contains("featured collection"),
            "and an activity with no target names none"
        );

        let no_featured_url = handle_add(
            state,
            view(
                "Add",
                "add-5",
                MALLORY,
                json!({"object": bobs_post.uri.clone(), "target": FEATURED}),
            ),
            mallory,
        )
        .await;
        assert!(
            ignored(no_featured_url).contains("featured collection"),
            "an actor whose document named no featured url matches no target"
        );
    }

    /// The `Add`/`Remove` object arm: an unknown status, an object that
    /// names no id, and somebody else's post are all ignored — on both
    /// routes — and nothing is pinned.
    #[tokio::test]
    async fn an_add_or_remove_of_a_status_it_may_not_pin_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "pin-bob").await;
        let bobs_post = local_post(&pool, &bob, "not alice's").await;

        let unknown_status = handle_add(
            state.clone(),
            view(
                "Add",
                "add-6",
                ALICE,
                json!({"object": "https://elsewhere.example/posts/never", "target": FEATURED}),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(unknown_status).contains("not a stored status"),
            "an unknown status on another host is never fetched"
        );

        let idless_object = handle_add(
            state.clone(),
            view(
                "Add",
                "add-8",
                ALICE,
                json!({"object": {"type": "Note", "content": "no id"}, "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(idless_object).contains("names no id"),
            "an Add whose embedded object carries no id resolves to nothing to pin"
        );

        let foreign_post = handle_add(
            state.clone(),
            view(
                "Add",
                "add-7",
                ALICE,
                json!({"object": bobs_post.uri.clone(), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(foreign_post).contains("did not author"),
            "an actor pins its own posts only"
        );

        let unpin_foreign = handle_remove(
            state,
            view(
                "Remove",
                "remove-3",
                ALICE,
                json!({"object": bobs_post.uri.clone(), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(unpin_foreign).contains("did not author"),
            "and the same authorship rule guards the Remove"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pinned_statuses")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "none of them pinned anything"
        );
    }
}
