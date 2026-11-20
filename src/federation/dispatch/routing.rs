//! The registry dispatch: the by-type route, the `Update` and `Delete` arms
//! that route by the type of the object they carry, and the handler call
//! every arm ends in. The `Create` and `Undo` arms, the two with
//! pre-checks and lookups of their own, live in the children.

use tracing::debug;

use super::{
    Handlers,
    error::job_error_from,
    types::{ActivityView, HandlerFn, Outcome},
};
use crate::{federation::json, jobs::JobError, models::user::User, state::AppState};

mod create;
mod undo;

use create::route_create;
use undo::route_undo;

/// The `ActivityStreams` type of withdrawal activities, the one type the
/// dispatcher does not route by its own name but by the type of the
/// activity it undoes.
const UNDO_TYPE: &str = "Undo";

/// The `ActivityStreams` type of editing activities: like `Undo`, routed
/// one level deeper than its own name — by the type of the object it
/// carries, through [`Handlers::update_by_object_type`].
const UPDATE_TYPE: &str = "Update";

/// The `ActivityStreams` type of deletion activities, the other
/// object-type arm of [`route`], keyed through
/// [`Handlers::delete_by_object_type`] with [`Handlers::delete_fallback`]
/// behind it.
const DELETE_TYPE: &str = "Delete";

/// The `ActivityStreams` type of announcement activities: the third
/// object-type arm of [`route`], keyed through
/// [`Handlers::create_by_object_type`] — the code that folds a created
/// `Note` and the code that will fold a created `ChatMessage` have nothing
/// in common beyond the `Create` around them, so the object's own type
/// routes.
const CREATE_TYPE: &str = "Create";

/// The registry dispatch: `Undo` routes by its inner activity's type,
/// `Update` and `Delete` by the type of the object they carry, and every
/// other type by its own name; an unregistered type is ignored.
pub(super) async fn route(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    match view.kind.as_str() {
        UNDO_TYPE => route_undo(state, view, signer, handlers).await,
        UPDATE_TYPE => route_update(state, view, signer, handlers).await,
        DELETE_TYPE => route_delete(state, view, signer, handlers).await,
        CREATE_TYPE => route_create(state, view, signer, handlers).await,
        _ => route_by_type(state, view, signer, handlers).await,
    }
}

/// The plain arm of the dispatch: the activity's own type, or nothing.
async fn route_by_type(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    match handlers.by_type.get(view.kind.as_str()) {
        None => {
            debug!(kind = %view.kind, "no handler for this activity type; ignoring");
            Ok(())
        }
        Some(handler) => call_handler(*handler, state, view, signer).await,
    }
}

/// The `Update` arm: the embedded object's own type decides which code
/// folds it, because an updated actor and an updated status have nothing in
/// common beyond the activity around them. An object that is a bare
/// reference declares no type and is ignored, and so is any type no ticket
/// has registered a route for yet.
async fn route_update(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    let declared = object_type(&view);
    let handler = declared
        .as_deref()
        .and_then(|kind| handlers.update_by_object_type.get(kind))
        .copied();
    if let Some(handler) = handler {
        call_handler(handler, state, view, signer).await
    } else {
        debug!(
            object_type = declared.as_deref().unwrap_or("<reference>"),
            "no Update handler for this object type; ignoring"
        );
        Ok(())
    }
}

/// The `Delete` arm: an embedded document whose type a sub-registry entry
/// claims goes to that entry; everything else — a bare id, a `Tombstone`, a
/// type nobody registered — goes to [`Handlers::delete_fallback`], which
/// resolves the id and decides whether it names an account. With no
/// fallback registered the activity is ignored.
async fn route_delete(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    let declared = object_type(&view);
    let handler = declared
        .as_deref()
        .and_then(|kind| handlers.delete_by_object_type.get(kind))
        .or(handlers.delete_fallback.as_ref())
        .copied();
    if let Some(handler) = handler {
        call_handler(handler, state, view, signer).await
    } else {
        debug!(
            object_type = declared.as_deref().unwrap_or("<reference>"),
            "no Delete handler for this object; ignoring"
        );
        Ok(())
    }
}

/// The type an activity's object declares, `None` when the object is a
/// bare reference: the key both object-type sub-registries route on.
fn object_type(view: &ActivityView) -> Option<String> {
    view.object
        .embedded()
        .and_then(|object| object.get("type"))
        .and_then(json::first_type)
}

/// Runs one handler inside the activity's span and maps its failure onto
/// the job's treatment.
async fn call_handler(
    handler: HandlerFn,
    state: &AppState,
    view: ActivityView,
    signer: User,
) -> Result<(), JobError> {
    match handler(state.clone(), view, signer).await {
        Ok(Outcome::Handled) => {
            debug!("the activity was handled");
            Ok(())
        }
        Ok(Outcome::Ignored(reason)) => {
            debug!(reason, "the handler ignored the activity");
            Ok(())
        }
        Err(err) => Err(job_error_from(err)),
    }
}

#[cfg(test)]
mod tests {
    use futures::future::BoxFuture;
    use serde_json::json;

    use super::*;
    use crate::federation::{
        dispatch::{
            FederationError, dispatch_activity,
            test_support::{
                app_state, deliver, incoming_args, ledger_rows, ok_handler, record, seen, signer,
            },
        },
        mrf,
    };

    /// The `Update` fake for an actor object.
    fn update_person_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("update-person", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Delete` fake for an object type the sub-registry claims.
    fn delete_tombstone_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("delete-tombstone", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Delete` fake for the fallback: the route that resolves an id.
    fn delete_fallback_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("delete-fallback", &view);
            Ok(Outcome::Handled)
        })
    }

    /// An unknown type reaches no handler and costs nothing: the job
    /// completes and the ledger marks the id seen.
    #[tokio::test]
    async fn unknown_types_are_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/unknown-1",
            "type": "Offer",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        });

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &Handlers::new(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "an unroutable activity completes the job");
        assert_eq!(
            seen("ok", "https://peer.example/activities/unknown-1"),
            0,
            "no handler ran"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/unknown-1").await,
            1,
            "and the id is spent: it will not be re-ignored forever"
        );
    }

    /// The `Update` arm of the dispatch: the type of the embedded object
    /// decides the route, and the activity's own type table is never
    /// consulted for it.
    #[tokio::test]
    async fn an_update_routes_by_its_objects_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register("Update", ok_handler);
        handlers.register_update("Person", update_person_handler);
        let activity = json!({
            "id": "https://peer.example/activities/update-person",
            "type": "Update",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/users/alice",
                "type": "Person",
                "name": "Alice again",
            },
        });

        let folded = deliver(&app_state(&pool), &alice, &activity, &handlers).await;

        assert_eq!(folded, Ok(()), "the actor update completes");
        assert_eq!(
            seen(
                "update-person",
                "https://peer.example/activities/update-person"
            ),
            1,
            "the object's own type decided the route"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/update-person"),
            0,
            "the activity type table was not consulted for an Update"
        );
    }

    /// An `Update` of an object type no ticket has registered a route for —
    /// or of an object that is only a reference and so declares no type —
    /// reaches nobody and completes the job.
    #[tokio::test]
    async fn an_update_of_an_unclaimed_object_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let handlers = Handlers::new();
        let note_update = json!({
            "id": "https://peer.example/activities/update-note",
            "type": "Update",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/1",
                "type": "Note",
                "content": "edited, but nobody here folds notes yet",
            },
        });
        let bare_update = json!({
            "id": "https://peer.example/activities/update-bare",
            "type": "Update",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/users/alice",
        });
        let state = app_state(&pool);

        let unfolded = deliver(&state, &alice, &note_update, &handlers).await;
        let unreferenced = deliver(&state, &alice, &bare_update, &handlers).await;

        assert_eq!(
            unfolded,
            Ok(()),
            "an unregistered object type completes as an ignore"
        );
        assert_eq!(
            unreferenced,
            Ok(()),
            "and so does an object that declares no type"
        );
        assert_eq!(
            seen(
                "update-person",
                "https://peer.example/activities/update-note"
            ) + seen("ok", "https://peer.example/activities/update-note")
                + seen(
                    "update-person",
                    "https://peer.example/activities/update-bare"
                )
                + seen("ok", "https://peer.example/activities/update-bare"),
            0,
            "neither delivery reached a handler"
        );
    }

    /// The `Delete` arm: an embedded document whose type the sub-registry
    /// claims goes to that claimant; one it does not claim — an unclaimed
    /// type or a bare id — falls to the route that resolves the id.
    #[tokio::test]
    async fn a_delete_routes_by_its_objects_type_or_through_the_fallback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register_delete("Tombstone", delete_tombstone_handler);
        handlers.register_delete_fallback(delete_fallback_handler);
        let claimed_type = json!({
            "id": "https://peer.example/activities/delete-tombstone",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": {"type": "Tombstone", "id": "https://peer.example/posts/1"},
        });
        let unclaimed_type = json!({
            "id": "https://peer.example/activities/delete-article",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": {"type": "Article", "id": "https://peer.example/articles/1"},
        });
        let bare = json!({
            "id": "https://peer.example/activities/delete-bare",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/users/alice",
        });
        let state = app_state(&pool);

        let claimed = deliver(&state, &alice, &claimed_type, &handlers).await;
        let other = deliver(&state, &alice, &unclaimed_type, &handlers).await;
        let resolved = deliver(&state, &alice, &bare, &handlers).await;

        assert_eq!(claimed, Ok(()), "the claimed type's route ran");
        assert_eq!(other, Ok(()), "an unclaimed type still falls back");
        assert_eq!(resolved, Ok(()), "and so does a bare id");
        assert_eq!(
            seen(
                "delete-tombstone",
                "https://peer.example/activities/delete-tombstone"
            ),
            1,
            "the embedded document went to its type's entry"
        );
        assert_eq!(
            seen(
                "delete-fallback",
                "https://peer.example/activities/delete-bare"
            ) + seen(
                "delete-fallback",
                "https://peer.example/activities/delete-article"
            ),
            2,
            "the id and the unclaimed type both reached the fallback"
        );
        assert_eq!(
            seen(
                "delete-tombstone",
                "https://peer.example/activities/delete-bare"
            ) + seen(
                "delete-tombstone",
                "https://peer.example/activities/delete-article"
            ),
            0,
            "and neither reached the wrong entry"
        );
    }

    /// A `Delete` arriving at a registry with no route for it — neither a
    /// type claim nor a fallback — is ignored, not failed.
    #[tokio::test]
    async fn a_delete_without_any_route_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/delete-no-routes",
            "type": "Delete",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/users/alice",
        });

        let outcome = deliver(&app_state(&pool), &alice, &activity, &Handlers::new()).await;

        assert_eq!(outcome, Ok(()), "an unroutable Delete completes the job");
        assert_eq!(
            seen(
                "delete-fallback",
                "https://peer.example/activities/delete-no-routes"
            ),
            0,
            "and reached no handler"
        );
    }
}
