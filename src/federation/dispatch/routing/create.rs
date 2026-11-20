//! The `Create` arm of the dispatch: routing by the type of the embedded
//! object, through the pre-checks and the bare-reference route.

use tracing::debug;

use super::{call_handler, object_type};
use crate::{
    federation::dispatch::{ActivityView, Handlers},
    jobs::JobError,
    models::user::User,
    state::AppState,
};

/// The `Create` arm: the embedded object's own type decides which code
/// folds it — a `Note` becomes a status, a later ticket's `ChatMessage`
/// becomes a chat message — through [`Handlers::create_by_object_type`].
/// A type that carries more than one kind of thing is put to its
/// [pre-checks](Handlers::register_create_prechecked) first, in registration
/// order, and a claimed document leaves through the guard's own handler.
/// An object that is a bare reference declares no type; it goes to
/// [`Handlers::create_by_reference`], whose handler dereferences the id and
/// folds what answers — with no reference handler registered such a
/// `Create` is ignored, as is any type no ticket has routed yet.
pub(super) async fn route_create(
    state: &AppState,
    view: ActivityView,
    signer: User,
    handlers: &Handlers,
) -> Result<(), JobError> {
    if view.object.embedded().is_none() {
        if let Some(handler) = handlers.create_by_reference {
            return call_handler(handler, state, view, signer).await;
        }
        debug!("no Create handler takes a bare object reference; ignoring");
        return Ok(());
    }
    let declared = object_type(&view);
    let claimed = match (declared.as_deref(), view.object.embedded()) {
        (Some(kind), Some(object)) => handlers.claimed_create(kind, state, object).await,
        (Some(_), None) | (None, _) => None,
    };
    if let Some(handler) = claimed {
        return call_handler(handler, state, view, signer).await;
    }
    let handler = declared
        .as_deref()
        .and_then(|kind| handlers.create_by_object_type.get(kind))
        .copied();
    if let Some(handler) = handler {
        call_handler(handler, state, view, signer).await
    } else {
        debug!(
            object_type = declared.as_deref().unwrap_or("<untyped>"),
            "no Create handler for this object type; ignoring"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use futures::future::BoxFuture;
    use serde_json::{Value, json};

    use super::*;
    use crate::federation::dispatch::{
        FederationError, Outcome,
        test_support::{app_state, deliver, ok_handler, record, seen, signer},
    };

    /// The `Create` pre-check fake of the vote ticket's guard: it claims every
    /// document that names an option and passes over every other one.
    fn claims_named(_state: AppState, object: &Value) -> BoxFuture<'static, bool> {
        let named = object
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty());
        Box::pin(async move { named })
    }

    /// The handler the [`claims_named`] guard routes to.
    fn prechecked_handler(
        _state: AppState,
        view: ActivityView,
        _signer: User,
    ) -> BoxFuture<'static, Result<Outcome, FederationError>> {
        Box::pin(async move {
            record("prechecked", &view);
            Ok(Outcome::Handled)
        })
    }

    /// The `Create` arm of the dispatch: the embedded object's own type
    /// decides the route, a bare reference and an unclaimed type reach
    /// nobody, and the activity type table is never consulted for a
    /// `Create` either.
    #[tokio::test]
    async fn a_create_routes_by_its_objects_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register("Create", ok_handler);
        handlers.register_create("Note", ok_handler);
        let embedded_note = json!({
            "id": "https://peer.example/activities/create-note",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/cn",
                "type": "Note",
                "content": "a note to fold",
            },
        });
        let bare = json!({
            "id": "https://peer.example/activities/create-bare",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/posts/cb",
        });
        let unclaimed = json!({
            "id": "https://peer.example/activities/create-chat",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/cc",
                "type": "ChatMessage",
                "content": "no chat ticket has claimed this yet",
            },
        });
        let state = app_state(&pool);

        let folded = deliver(&state, &alice, &embedded_note, &handlers).await;
        let referenced = deliver(&state, &alice, &bare, &handlers).await;
        let unknown = deliver(&state, &alice, &unclaimed, &handlers).await;

        assert_eq!(folded, Ok(()), "the registered object type ran its handler");
        assert_eq!(
            referenced,
            Ok(()),
            "a bare reference completes as an ignore"
        );
        assert_eq!(unknown, Ok(()), "and so does an unclaimed object type");
        assert_eq!(
            seen("ok", "https://peer.example/activities/create-note"),
            1,
            "the object's own type decided the route"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/create-bare")
                + seen("ok", "https://peer.example/activities/create-chat"),
            0,
            "neither the reference nor the unclaimed type reached a handler"
        );
    }

    /// The reference arm of the `Create` dispatch once a ticket registers
    /// it: a bare-id object reaches the `create_by_reference` handler.
    #[tokio::test]
    async fn a_create_of_a_bare_reference_reaches_the_registered_reference_handler() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register_create_reference(ok_handler);
        let activity = json!({
            "id": "https://peer.example/activities/create-ref",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": "https://peer.example/posts/ref",
        });
        let state = app_state(&pool);

        let delivered = deliver(&state, &alice, &activity, &handlers).await;

        assert_eq!(
            delivered,
            Ok(()),
            "the reference handler completing ends the job"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/create-ref"),
            1,
            "the bare-id Create reached the reference route"
        );
    }

    /// A `Create` pre-check runs ahead of its object type's entry: a document
    /// the guard claims leaves through the guard's own handler, and one it
    /// passes over folds through the table exactly as it did before the guard
    /// existed. That is the shape of the poll vote — a `Note` that is not a
    /// post — and the fall-through is what keeps ordinary notes alone.
    #[tokio::test]
    async fn a_create_pre_check_runs_ahead_of_its_object_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let mut handlers = Handlers::new();
        handlers.register_create("Note", ok_handler);
        handlers.register_create_prechecked("Note", claims_named, prechecked_handler);
        let named = json!({
            "id": "https://peer.example/activities/guarded-note",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/guarded",
                "type": "Note",
                "name": "the claimed option",
            },
        });
        let plain = json!({
            "id": "https://peer.example/activities/unguarded-note",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://peer.example/posts/unguarded",
                "type": "Note",
                "content": "an ordinary post",
            },
        });
        let state = app_state(&pool);

        let claimed = deliver(&state, &alice, &named, &handlers).await;
        let passed = deliver(&state, &alice, &plain, &handlers).await;

        assert_eq!(claimed, Ok(()), "the claimed document completes");
        assert_eq!(passed, Ok(()), "and so does the passed-over one");
        assert_eq!(
            seen("prechecked", "https://peer.example/activities/guarded-note"),
            1,
            "the guard's handler folded the document it claimed"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/guarded-note"),
            0,
            "and the type's own entry never saw it"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/unguarded-note"),
            1,
            "the document the guard passed over reached the table"
        );
        assert_eq!(
            seen(
                "prechecked",
                "https://peer.example/activities/unguarded-note"
            ),
            0,
            "and not the guarded handler"
        );
    }
}
