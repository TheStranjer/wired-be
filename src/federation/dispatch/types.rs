//! The vocabulary the dispatcher and its handlers share: the [`Outcome`] a
//! handler reports, the normalized [`ActivityView`] and [`ObjectRef`] it is
//! handed, and the function-pointer types the registry stores.

use futures::future::BoxFuture;
use serde_json::Value;
use uuid::Uuid;

use super::error::FederationError;
use crate::{
    federation::{json, mrf},
    models::user::User,
    state::AppState,
};

/// Everything the dispatcher can do with a received activity, as the
/// per-type handlers report it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The handler folded the activity; the job completes.
    Handled,
    /// The handler saw nothing to do; the reason is static — recorded in
    /// the debug log, the job completes the same.
    Ignored(&'static str),
}

/// The `object` of a normalized activity.
#[derive(Clone, Debug)]
pub enum ObjectRef {
    /// A reference to an object kept elsewhere: a bare id string, or an
    /// object/array shape whose first reference ([`json::value_or_id`])
    /// wins.
    Id(String),
    /// A full embedded document, kept verbatim for the handlers that fold
    /// objects (and inspectable through [`ObjectRef::id`] for the id it
    /// carries).
    Embedded(Value),
}

impl ObjectRef {
    /// Normalizes one `object` field value: an object is embedded, an
    /// array recurses into its first element (the one-or-many rule), and
    /// anything else must carry a reference to be an [`ObjectRef::Id`].
    /// `None` — for a missing, null, or reference-less object — means the
    /// activity describes nothing the dispatcher could route.
    #[must_use]
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Object(_) => Some(Self::Embedded(value.clone())),
            Value::Array(values) => values.first().and_then(Self::from_value),
            Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {
                json::value_or_id(value).map(Self::Id)
            }
        }
    }

    /// The id this object names: the string of a reference, or the `id`
    /// field of an embedded document (an embedded document may carry none,
    /// which is why this answers an `Option`).
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Id(id) => Some(id.as_str()),
            Self::Embedded(object) => object.get("id").and_then(Value::as_str),
        }
    }

    /// The embedded document, for the handlers that fold objects.
    #[must_use]
    pub const fn embedded(&self) -> Option<&Value> {
        match self {
            Self::Embedded(object) => Some(object),
            Self::Id(_) => None,
        }
    }
}

/// A received activity normalized through the [`json`] helpers: every
/// field the wire protocol may send as a string, an object, or an array
/// arrives here in exactly one shape.
///
/// A type handler therefore never branches on JSON cardinality.
#[derive(Clone, Debug)]
pub struct ActivityView {
    /// The activity's `ActivityPub` id — the dedupe key and the subject
    /// of `Delete`/`Undo` chains.
    pub id: String,
    /// The activity type: the first string of a `type` field that may
    /// itself be an array.
    pub kind: String,
    /// The acting actor's id — a string, an actor object's `id`, or the
    /// first reference of an array, normalized to the id alone.
    pub actor: String,
    /// The activity's object: a reference or an embedded document.
    pub object: ObjectRef,
    /// The `target` of `Add`/`Remove`/`Move` activities, normalized to a
    /// reference; `None` when the activity carries none.
    pub target: Option<String>,
    /// The `to` addressing, every element normalized to a reference.
    pub to: Vec<String>,
    /// The `cc` addressing, every element normalized to a reference.
    pub cc: Vec<String>,
    /// The document as received, for handlers that need fields beyond the
    /// normalized five and for the MRF's re-normalization.
    pub raw: Value,
    /// The `users.id` of the local account whose personal inbox received
    /// the delivery; `None` for the shared inboxes and for views not built
    /// from a delivery. Parsing a document leaves it `None`; the
    /// dispatcher sets it from the job args.
    pub inbox_owner: Option<Uuid>,
}

impl ActivityView {
    /// Normalizes a received document, `None` when it is not a dispatch-
    /// able activity: an `id` string, a first `type` string, an `actor`
    /// reference and an `object` are the minimum the dispatcher's own
    /// steps (containment, dedupe, routing) all need.
    #[must_use]
    pub(crate) fn parse(raw: &Value) -> Option<Self> {
        let id = raw.get("id").and_then(Value::as_str)?;
        let kind = raw.get("type").and_then(json::first_type)?;
        let actor = raw.get("actor").and_then(json::value_or_id)?;
        let object = raw.get("object").and_then(ObjectRef::from_value)?;
        Some(Self {
            id: id.to_owned(),
            kind,
            actor,
            object,
            target: raw.get("target").and_then(json::value_or_id),
            to: raw.get("to").map_or_else(Vec::new, json::references),
            cc: raw.get("cc").map_or_else(Vec::new, json::references),
            raw: raw.clone(),
            inbox_owner: None,
        })
    }
}

/// What a per-type handler does with a normalized activity: folds it,
/// ignores it, or fails with a [`FederationError`] the dispatcher maps
/// onto the job.
pub type HandlerFn =
    fn(AppState, ActivityView, User) -> BoxFuture<'static, Result<Outcome, FederationError>>;

/// What an `Undo` handler does with an `Undo` whose inner activity has
/// already been resolved to its type and checked against its actor: the
/// same contract as a [`HandlerFn`], registered per inner type.
pub type UndoFn = HandlerFn;

/// What a `Create` pre-check decides about an embedded object: `true` claims
/// the activity for the handler registered beside the guard, `false` leaves
/// it to the object type's own entry.
///
/// A guard that must consult the database takes the cheap `AppState` by
/// value and copies whatever it needs out of the document before its first
/// `await`, exactly like [`mrf::filter_incoming_boxed`] — that is why the
/// document arrives borrowed while the future may not be.
///
/// [`mrf::filter_incoming_boxed`]: crate::federation::mrf::filter_incoming_boxed
pub type CreatePreCheck = fn(AppState, &Value) -> BoxFuture<'static, bool>;

/// One guarded `Create` route: the object type whose entry it runs ahead of,
/// the guard, and the handler the guard hands a claimed activity to.
#[derive(Clone, Copy, Debug)]
pub(super) struct CreateGuard {
    /// The declared object `type` this guard is consulted for — the same key
    /// [`Handlers::register_create`] binds.
    pub(super) object_type: &'static str,
    /// Whether the delivered document is the specialised shape.
    pub(super) check: CreatePreCheck,
    /// The handler a claimed document routes to.
    pub(super) handler: HandlerFn,
}

/// The policy stage of the pipeline, as a function pointer so the
/// dispatcher's test seam can hand it a rejecting filter.
pub(super) type MrfFn = fn(AppState, &ActivityView) -> BoxFuture<'static, mrf::MrfDecision>;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn normalization_reads_every_field_shape() {
        let view = ActivityView::parse(&json!({
            "id": "https://peer.example/activities/1",
            "type": ["Create", "Service"],
            "actor": [{"id": "https://peer.example/users/alice", "type": "Person"}],
            "object": {"id": "https://peer.example/posts/1", "type": "Note"},
            "target": {"id": "https://peer.example/collections/featured"},
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [{"type": "Collection", "id": "https://peer.example/users/alice/followers"},
                    "https://peer.example/users/bob"],
        }))
        .expect("the fixture parses");
        assert_eq!(view.kind, "Create", "the first string of the array type");
        assert_eq!(
            view.actor, "https://peer.example/users/alice",
            "an actor object inside an array reads as its id"
        );
        assert!(
            matches!(view.object, ObjectRef::Embedded(_)),
            "an embedded object stays a document"
        );
        assert_eq!(
            view.object.id(),
            Some("https://peer.example/posts/1"),
            "and the id it carries reads off it"
        );
        assert_eq!(
            view.target.as_deref(),
            Some("https://peer.example/collections/featured"),
            "an object target reads as its id"
        );
        assert_eq!(view.to.len(), 1, "the one-element addressing array");
        assert_eq!(
            view.cc,
            vec![
                "https://peer.example/users/alice/followers".to_owned(),
                "https://peer.example/users/bob".to_owned()
            ],
            "a mixed cc array normalizes element by element"
        );

        let referenced = ActivityView::parse(&json!({
            "id": "https://peer.example/activities/2",
            "type": "Announce",
            "actor": ["https://peer.example/users/alice"],
            "object": ["https://peer.example/posts/1"],
        }))
        .expect("the announce parses");
        assert!(
            matches!(&referenced.object, ObjectRef::Id(id)
                if id == "https://peer.example/posts/1"),
            "an array of references reads as the head reference"
        );
        assert!(referenced.target.is_none(), "no target, no reference");
        assert!(
            referenced.to.is_empty() && referenced.cc.is_empty(),
            "addressing fields that are absent normalize to empty"
        );
    }

    /// An activity the dispatcher cannot route — no id, no type, no
    /// actor, no object — does not parse.
    #[test]
    fn normalization_requires_the_four_routing_fields() {
        let complete = json!({
            "id": "https://peer.example/activities/3",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        });
        assert!(ActivityView::parse(&complete).is_some(), "the base case");
        for field in ["id", "type", "actor", "object"] {
            let mut missing = complete.as_object().expect("an object").clone();
            let _gone: Option<Value> = missing.remove(field);
            assert!(
                ActivityView::parse(&Value::Object(missing)).is_none(),
                "no view without {field}"
            );
        }
        assert!(
            ActivityView::parse(&json!({
                "id": "https://peer.example/activities/4",
                "type": "Follow",
                "actor": "https://peer.example/users/alice",
                "object": 7,
            }))
            .is_none(),
            "an object that is neither a document nor a reference cannot route"
        );
        let idless = ActivityView::parse(&json!({
            "id": "https://peer.example/activities/5",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "object": {"type": "Note"},
        }))
        .expect("an embedded object without an id still parses");
        assert!(
            idless.object.id().is_none(),
            "and says so through ObjectRef::id"
        );
    }
}
