//! The `Undo` arm of the dispatch: resolving the activity being undone,
//! checking its actor, and routing by its type.

use tracing::debug;

use super::call_handler;
use crate::{
    federation::{
        dispatch::{ActivityView, Handlers, ObjectRef},
        json,
    },
    jobs::{JobError, JobErrorExt as _},
    models::{activity, user::User},
    state::AppState,
};

/// The `Undo` arm: resolve what is being undone, refuse an `Undo` whose
/// actor is not the actor of the activity it undoes, then route by the
/// inner type through the `Undo` table.
pub(super) async fn route_undo(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    let Some((inner_type, inner_actor)) = undo_inner(state, &view).await? else {
        return Ok(());
    };
    if inner_actor != view.actor {
        debug!(
            inner_actor = %inner_actor,
            "an Undo whose actor is not the actor of what it undoes; discarding"
        );
        return Ok(());
    }
    match handlers.undo_by_type.get(inner_type.as_str()) {
        None => {
            debug!(inner_type = %inner_type, "no Undo handler for the inner activity type; ignoring");
            Ok(())
        }
        Some(handler) => call_handler(*handler, state, view, signer).await,
    }
}

/// The inner activity of an `Undo` as `(type, actor id)`: an object that
/// names an id is looked up among the stored remote activities first (what
/// this instance stored is what it withdraws, whatever type the `Undo`
/// embeds — a Misskey reaction arrives as an embedded `Like` yet is stored
/// as an emoji reaction); an embedded object with no stored row states its
/// own type and actor. `None` (after the debug log that says why) is an
/// `Undo` of nothing this instance can route.
async fn undo_inner(
    state: &AppState,
    view: &ActivityView,
) -> Result<Option<(String, String)>, JobError> {
    if let ObjectRef::Embedded(object) = &view.object
        && let Some(id) = object.get("id").and_then(serde_json::Value::as_str)
    {
        let stored = activity::find_by_ap_id(&state.db, id)
            .await
            .job_retry("the undo lookup failed")?;
        if let Some((kind, actor)) = stored {
            return Ok(Some((kind.to_owned(), actor)));
        }
    }
    match &view.object {
        ObjectRef::Embedded(object) => {
            let Some(kind) = object.get("type").and_then(json::first_type) else {
                debug!("an Undo of an embedded object without a type; ignoring");
                return Ok(None);
            };
            let actor = object
                .get("actor")
                .and_then(json::value_or_id)
                .unwrap_or_else(|| view.actor.clone());
            Ok(Some((kind, actor)))
        }
        ObjectRef::Id(id) => {
            let stored = activity::find_by_ap_id(&state.db, id)
                .await
                .job_retry("the undo lookup failed")?;
            match stored {
                None => {
                    debug!(inner = %id, "an Undo of an activity this instance never stored; ignoring");
                    Ok(None)
                }
                Some((kind, actor)) => Ok(Some((kind.to_owned(), actor))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::federation::{
        dispatch::{
            Handlers, dispatch_activity,
            test_support::{
                app_state, incoming_args, local_user, seen, signer, test_handlers, undo_activity,
            },
        },
        mrf,
    };

    /// `Undo` routing, first arm: the embedded inner object states its
    /// own type, and the matching `Undo` handler runs.
    #[tokio::test]
    async fn an_undo_routes_by_its_embedded_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/undo-1",
            "type": "Undo",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/activities/follow-1",
                "type": "Follow",
                "actor": "https://peer.example/users/alice",
                "object": "https://test.example/users/bob",
            },
        });

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "the undo completes");
        assert_eq!(
            seen("undo-follow", "https://peer.example/activities/undo-1"),
            1,
            "and routed through the Follow undo handler"
        );
    }

    /// `Undo` routing, second arm: an object that is only an id learns
    /// its type from the stored rows — a follow by its `ap_id`, a reblog
    /// by its `Announce` uri — and each routes to its own handler.
    #[tokio::test]
    async fn an_undo_resolves_a_bare_id_through_the_stored_activities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let bob = local_user(&pool, "bob").await;
        let _followed = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state, ap_id)
             VALUES ($1, $2, $3, 'accepted', 'https://peer.example/activities/follow-stored')",
        )
        .bind(Uuid::now_v7())
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the follow fixture inserts");
        let original_id = Uuid::now_v7();
        let announce_id = Uuid::now_v7();
        for (id, uri, reblog_of) in [
            (original_id, "https://test.example/objects/post-9", None),
            (
                announce_id,
                "https://peer.example/activities/announce-stored",
                Some(original_id),
            ),
        ] {
            let _inserted = sqlx::query(
                "INSERT INTO statuses (id, uri, user_id, conversation_id, local, visibility, reblog_of_id)
                 VALUES ($1, $2, $3, $1, false, 'public', $4)",
            )
            .bind(id)
            .bind(uri)
            .bind(alice.id)
            .bind(reblog_of)
            .execute(&pool)
            .await
            .expect("the status fixtures insert");
        }
        let handlers = test_handlers();

        let undo_follow = undo_activity("undo-2", "https://peer.example/activities/follow-stored");
        let follow_outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&undo_follow, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(follow_outcome, Ok(()), "the follow undo completes");

        let undo_announce =
            undo_activity("undo-3", "https://peer.example/activities/announce-stored");
        let announce_outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&undo_announce, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(announce_outcome, Ok(()), "the announce undo completes");

        assert_eq!(
            seen("undo-follow", "https://peer.example/activities/undo-2"),
            1,
            "the stored follow routed by ap_id"
        );
        assert_eq!(
            seen("undo-announce", "https://peer.example/activities/undo-3"),
            1,
            "the stored reblog routed by its Announce uri"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/undo-2")
                + seen("ok", "https://peer.example/activities/undo-3"),
            0,
            "nothing routed as a plain activity"
        );
    }

    /// An `Undo` stored under someone else's actor, and an `Undo` of an
    /// id nothing stores, both route to nobody.
    #[tokio::test]
    async fn undos_of_foreign_and_unknown_ids_are_dropped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let bob = local_user(&pool, "bob").await;
        let _followed = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state, ap_id)
             VALUES ($1, $2, $3, 'accepted', 'https://peer.example/activities/bobs-follow')",
        )
        .bind(Uuid::now_v7())
        .bind(bob.id)
        .bind(alice.id)
        .execute(&pool)
        .await
        .expect("the follow fixture inserts");
        let state = app_state(&pool);
        let handlers = test_handlers();

        // bob's follow, undone by alice: the actors differ, so it stops.
        let foreign = undo_activity("undo-6", "https://peer.example/activities/bobs-follow");
        let foreign_outcome = dispatch_activity(
            &state,
            incoming_args(&foreign, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(
            foreign_outcome,
            Ok(()),
            "the actor mismatch completes the job"
        );

        let unknown = undo_activity("undo-7", "https://peer.example/activities/never-stored");
        let unknown_outcome = dispatch_activity(
            &state,
            incoming_args(&unknown, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(unknown_outcome, Ok(()), "and so does the unknown id");

        assert_eq!(
            seen("undo-follow", "https://peer.example/activities/undo-6")
                + seen("undo-follow", "https://peer.example/activities/undo-7"),
            0,
            "neither undo reached a handler"
        );
    }

    /// A public post by a fresh local account, for a remote actor to like.
    async fn target_post(pool: &sqlx::PgPool) -> crate::models::status::Status {
        let author = local_user(pool, "carol").await;
        crate::models::status::insert(
            pool,
            "https://test.example",
            &crate::models::status::NewStatus::local(&author, "react to me"),
        )
        .await
        .expect("the status fixture inserts")
    }

    /// How many emoji reaction rows exist.
    async fn reaction_rows(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM emoji_reactions")
            .fetch_one(pool)
            .await
            .expect("the table reads")
    }

    /// How many favourite rows exist.
    async fn favourite_rows(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM favourites")
            .fetch_one(pool)
            .await
            .expect("the table reads")
    }

    /// A Misskey-family reaction arrives as a `Like` with `content` and is
    /// stored as an emoji reaction; the `Undo` that embeds that `Like`
    /// resolves through the stored row, not the embedded type.
    #[tokio::test]
    async fn an_undo_embedding_a_reaction_like_withdraws_the_reaction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let post = target_post(&pool).await;
        let handlers = Handlers::default();
        let like = json!({
            "id": "https://peer.example/activities/reaction-like-1",
            "type": "Like",
            "actor": "https://peer.example/users/alice",
            "object": post.uri,
            "content": "🍮",
        });
        let delivered = dispatch_activity(
            &app_state(&pool),
            incoming_args(&like, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(delivered, Ok(()), "the reaction delivers");
        assert_eq!(reaction_rows(&pool).await, 1, "and is stored");

        let undo = json!({
            "id": "https://peer.example/activities/undo-reaction-1",
            "type": "Undo",
            "actor": "https://peer.example/users/alice",
            "object": like,
        });
        let undone = dispatch_activity(
            &app_state(&pool),
            incoming_args(&undo, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(undone, Ok(()), "the undo completes");
        assert_eq!(reaction_rows(&pool).await, 0, "the reaction is gone");
    }

    /// An embedded `Like` that is a plain favourite still unfavourites.
    #[tokio::test]
    async fn an_undo_embedding_a_plain_like_still_unfavourites() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let post = target_post(&pool).await;
        let handlers = Handlers::default();
        let like = json!({
            "id": "https://peer.example/activities/plain-like-1",
            "type": "Like",
            "actor": "https://peer.example/users/alice",
            "object": post.uri,
        });
        let delivered = dispatch_activity(
            &app_state(&pool),
            incoming_args(&like, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(delivered, Ok(()), "the like delivers");
        assert_eq!(favourite_rows(&pool).await, 1, "and is stored");

        let undo = json!({
            "id": "https://peer.example/activities/undo-plain-like-1",
            "type": "Undo",
            "actor": "https://peer.example/users/alice",
            "object": like,
        });
        let undone = dispatch_activity(
            &app_state(&pool),
            incoming_args(&undo, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(undone, Ok(()), "the undo completes");
        assert_eq!(favourite_rows(&pool).await, 0, "the favourite is gone");
    }

    /// An embedded `Follow` whose id nothing stores falls back to the
    /// embedded type and still routes to the Follow undo handler.
    #[tokio::test]
    async fn an_undo_embedding_an_unknown_follow_still_routes_by_embedded_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/undo-unknown-follow",
            "type": "Undo",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/activities/never-stored-follow",
                "type": "Follow",
                "actor": "https://peer.example/users/alice",
                "object": "https://test.example/users/bob",
            },
        });

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "the undo completes");
        assert_eq!(
            seen(
                "undo-follow",
                "https://peer.example/activities/undo-unknown-follow"
            ),
            1,
            "and routed through the Follow undo handler"
        );
    }
}
