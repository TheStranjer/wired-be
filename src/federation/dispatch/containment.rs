//! The containment stage of the pipeline: an activity must come from the
//! actor that signed it, carry an id on that actor's host, and not smuggle
//! in an embedded object from somewhere else.

use serde_json::Value;

use super::types::{ActivityView, ObjectRef};
use crate::{federation::json, models::user::User};

/// Activity types whose `object` is by design a reference to content that
/// lives somewhere else — someone else's note, actor, or custom emoji, or,
/// for the follow handshake's `Accept` and `Reject`, our *own* activity
/// handed back by the remote peer — so the containment rule of the embedded
/// object's host does not apply to them. Pleroma's `Transmogrifier` and
/// `ObjectValidators` treat exactly these as the activities that
/// legitimately speak about foreign objects.
const FOREIGN_OBJECT_TYPES: [&str; 12] = [
    "Announce",
    "Like",
    "EmojiReact",
    "Follow",
    "Accept",
    "Reject",
    "Block",
    "Flag",
    "Undo",
    "Delete",
    "Add",
    "Remove",
];

/// Where the activity could not belong to the actor that sent it — the
/// re-check of what the inbox already enforced, in the one place that
/// processes activities without the HTTP layer's guarantees. `None` means
/// it passes.
pub(super) fn containment_failure(view: &ActivityView, signer: &User) -> Option<String> {
    if view.actor != signer.ap_id {
        return Some(format!(
            "the activity actor {} is not the signer {}",
            view.actor, signer.ap_id
        ));
    }
    let Some(actor_host) = json::http_host(&view.actor) else {
        return Some(format!("the actor id {} is not an http(s) url", view.actor));
    };
    let id_host = json::http_host(&view.id);
    if id_host.as_deref() != Some(actor_host.as_str()) {
        return Some(format!(
            "the activity id host {} differs from the actor host {actor_host}",
            id_host.as_deref().unwrap_or("<none>")
        ));
    }
    object_containment_failure(view, &actor_host)
}

/// The embedded-object arm of containment: an object carried inside the
/// activity that names its own `id` must live on the actor's host, unless
/// the activity type is one that legitimately speaks about someone
/// else's content ([`FOREIGN_OBJECT_TYPES`]).
fn object_containment_failure(view: &ActivityView, actor_host: &str) -> Option<String> {
    let ObjectRef::Embedded(object) = &view.object else {
        return None;
    };
    let object_id = object.get("id").and_then(Value::as_str)?;
    let Some(object_host) = json::http_host(object_id) else {
        return Some(format!(
            "the embedded object id {object_id} is not an http(s) url"
        ));
    };
    if object_host == actor_host || FOREIGN_OBJECT_TYPES.contains(&view.kind.as_str()) {
        return None;
    }
    Some(format!(
        "the embedded object host {object_host} differs from the actor host {actor_host}"
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::federation::{
        dispatch::{
            dispatch_activity,
            test_support::{app_state, incoming_args, ledger_rows, seen, signer, test_handlers},
        },
        mrf,
    };

    /// Containment, first arm: an activity whose actor is not the signer
    /// is discarded before anything is written.
    #[tokio::test]
    async fn an_actor_that_is_not_the_signer_is_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = signer(&pool).await;
        let activity = json!({
            "id": "https://peer.example/activities/contain-1",
            "type": "Follow",
            "actor": "https://peer.example/users/mallory",
            "object": "https://test.example/users/bob",
        });

        let outcome = dispatch_activity(
            &app_state(&pool),
            incoming_args(&activity, &alice),
            &test_handlers(),
            mrf::filter_incoming_boxed,
        )
        .await;

        assert_eq!(outcome, Ok(()), "the job completes, silently");
        assert_eq!(
            seen("ok", "https://peer.example/activities/contain-1"),
            0,
            "nothing was routed"
        );
        assert_eq!(
            ledger_rows(&pool, "https://peer.example/activities/contain-1").await,
            0,
            "a discard leaves no ledger row"
        );
    }

    /// Containment, the host arms: an activity id on another host is
    /// dropped, a foreign embedded object is dropped, and the same
    /// foreign object under an exception-listed type passes.
    #[tokio::test]
    async fn ids_from_foreign_hosts_are_discarded_except_reference_objects() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = signer(&pool).await;
        let handlers = test_handlers();

        let wrong_id = json!({
            "id": "https://evil.example/activities/contain-2",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        });
        let dropped_id = dispatch_activity(
            &state,
            incoming_args(&wrong_id, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(dropped_id, Ok(()), "a mismatched activity id is dropped");

        let foreign_create = json!({
            "id": "https://peer.example/activities/contain-3",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://evil.example/posts/stolen",
                "type": "Note",
                "content": "not alice's",
            },
        });
        let dropped_create = dispatch_activity(
            &state,
            incoming_args(&foreign_create, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(
            dropped_create,
            Ok(()),
            "a Create of a foreign embedded object is dropped"
        );

        let foreign_announce = json!({
            "id": "https://peer.example/activities/contain-4",
            "type": "Announce",
            "actor": "https://peer.example/users/alice",
            "object": {
                "id": "https://other.example/posts/1",
                "type": "Note",
                "content": "someone else's post, announced",
            },
        });
        let announced = dispatch_activity(
            &state,
            incoming_args(&foreign_announce, &alice),
            &handlers,
            mrf::filter_incoming_boxed,
        )
        .await;
        assert_eq!(announced, Ok(()), "an Announce of a foreign object passes");
        assert_eq!(
            seen("ok", "https://peer.example/activities/contain-4"),
            1,
            "the exception list is what let it through"
        );
        assert_eq!(
            seen("ok", "https://peer.example/activities/contain-3"),
            0,
            "and the same object under Create did not"
        );
    }
}
