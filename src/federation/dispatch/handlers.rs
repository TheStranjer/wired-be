//! The type-to-handler tables the dispatcher routes through: the
//! [`Handlers`] registry, its registration methods, and the test-only
//! questions the handler modules ask of it.

use std::collections::HashMap;

use serde_json::Value;

use super::types::{CreateGuard, CreatePreCheck, HandlerFn, UndoFn};
use crate::state::AppState;

/// The type-to-handler tables the dispatcher routes through.
///
/// [`Handlers::default`] is the registry the live dispatcher runs: the
/// composition of every handler module in
/// [`crate::federation::handlers`]. A ticket that teaches the instance a
/// new activity type adds its routes to its own handler module, and nothing
/// here. [`Handlers::new`] is the seam the tests build fake registries with.
#[derive(Debug)]
pub struct Handlers {
    /// Routes keyed by the activity's own type.
    pub(super) by_type: HashMap<&'static str, HandlerFn>,
    /// `Undo` routes, keyed by the inner activity's type.
    pub(super) undo_by_type: HashMap<&'static str, UndoFn>,
    /// `Update` routes, keyed by the type of the *object* the activity
    /// carries: updating a `Person` and updating a `Note` are different
    /// code, and only the object's own type says which one a delivery
    /// needs.
    pub(super) update_by_object_type: HashMap<&'static str, HandlerFn>,
    /// `Delete` routes, keyed the same way, for the peers that send an
    /// embedded document.
    pub(super) delete_by_object_type: HashMap<&'static str, HandlerFn>,
    /// `Create` routes, keyed the same way: which code folds a delivered
    /// object depends on the object's own `type`, so a `Create` of a
    /// `Note` and the later `Create` of a `ChatMessage` register one
    /// entry each beside each other.
    pub(super) create_by_object_type: HashMap<&'static str, HandlerFn>,
    /// The `Create` route for an object that is only a reference: an
    /// embedded document declares a `type` to route on, a bare id declares
    /// nothing, and exactly one handler can take the whole shape — the one
    /// that dereferences the id and folds what it answers (see
    /// [`crate::federation::fetch_object`]).
    pub(super) create_by_reference: Option<HandlerFn>,
    /// `Create` pre-checks, in registration order: a delivered object of a
    /// keyed type is put to each of their guards before that type's entry in
    /// [`Handlers::create_by_object_type`] folds it, which is how one `type`
    /// that carries two different kinds of thing — a `Note` that is a post
    /// and a `Note` that is one option of a poll vote — reaches two
    /// different handlers.
    pub(super) create_prechecks: Vec<CreateGuard>,
    /// The `Delete` route for an object [`Handlers::delete_by_object_type`]
    /// does not claim — a bare id, or an embedded document of a type nobody
    /// registered. An id is the only thing that can tell a peer deleting
    /// its own account from one deleting a status, so that is where this
    /// resolution belongs.
    pub(super) delete_fallback: Option<HandlerFn>,
}

/// The live registry, built by the handler modules themselves.
impl Default for Handlers {
    fn default() -> Self {
        crate::federation::handlers::registry()
    }
}

impl Handlers {
    /// An empty registry: dispatching through it ignores everything. The
    /// tests' seam; [`Handlers::default`] is the one the worker runs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_type: HashMap::new(),
            undo_by_type: HashMap::new(),
            update_by_object_type: HashMap::new(),
            delete_by_object_type: HashMap::new(),
            create_by_object_type: HashMap::new(),
            create_by_reference: None,
            create_prechecks: Vec::new(),
            delete_fallback: None,
        }
    }

    /// Binds one activity type to its handler.
    pub fn register(&mut self, kind: &'static str, handler: HandlerFn) {
        let _previous = self.by_type.insert(kind, handler);
    }

    /// Binds the `Undo` of one inner activity type to its handler.
    pub fn register_undo(&mut self, inner_type: &'static str, handler: UndoFn) {
        let _previous = self.undo_by_type.insert(inner_type, handler);
    }

    /// Binds one `Update`d object type to its handler, filling the
    /// registry's `update_by_object_type` table.
    pub fn register_update(&mut self, object_type: &'static str, handler: HandlerFn) {
        let _previous = self.update_by_object_type.insert(object_type, handler);
    }

    /// Binds one `Delete`d object type to its handler, filling the
    /// registry's `delete_by_object_type` table.
    pub fn register_delete(&mut self, object_type: &'static str, handler: HandlerFn) {
        let _previous = self.delete_by_object_type.insert(object_type, handler);
    }

    /// Binds one `Create`d object type to its handler, filling the
    /// registry's `create_by_object_type` table.
    pub fn register_create(&mut self, object_type: &'static str, handler: HandlerFn) {
        let _previous = self.create_by_object_type.insert(object_type, handler);
    }

    /// Sets the handler a `Create` whose object is only a reference goes
    /// to — the [`register_delete_fallback`] of the `Create` route, for
    /// the code that dereferences the id instead of ignoring it.
    ///
    /// [`register_delete_fallback`]: Handlers::register_delete_fallback
    pub fn register_create_reference(&mut self, handler: HandlerFn) {
        self.create_by_reference = Some(handler);
    }

    /// Binds a guarded `Create` route: `check` puts the embedded document of
    /// a `Create`d `object_type` to its shape test, and a claimed
    /// activity goes to `handler` instead of to the type's entry in
    /// [`Handlers::register_create`].
    ///
    /// This is the "before" half of a shared object `type`: a poll vote
    /// arrives as a plain `Create` of a `Note` and only a read of the
    /// document (and, through its `inReplyTo`, of the database) tells it
    /// apart from the post it superficially is, so the note ticket's
    /// unguarded entry stays the fallback and the specialised ticket runs its
    /// guard first. Guards of one type run in registration order, and the
    /// first that claims the document ends the walk.
    pub fn register_create_prechecked(
        &mut self,
        object_type: &'static str,
        check: CreatePreCheck,
        handler: HandlerFn,
    ) {
        self.create_prechecks.push(CreateGuard {
            object_type,
            check,
            handler,
        });
    }

    /// The guarded route that claims this `Create`, `None` when no guard of
    /// `object_type` does: [`route_create`] runs the walk through here so the
    /// unguarded type table is consulted only once nothing claimed the
    /// document.
    ///
    /// [`route_create`]: super::routing::route_create
    pub(super) async fn claimed_create(
        &self,
        object_type: &str,
        state: &AppState,
        object: &Value,
    ) -> Option<HandlerFn> {
        for guard in &self.create_prechecks {
            if guard.object_type != object_type {
                continue;
            }
            if (guard.check)(state.clone(), object).await {
                return Some(guard.handler);
            }
        }
        None
    }

    /// Whether the by-name table claims `kind` — the
    /// [`handles_create`] question asked of the plain routes, by the
    /// handler modules that register them.
    ///
    /// [`handles_create`]: Self::handles_create
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles(&self, kind: &str) -> bool {
        self.by_type.contains_key(kind)
    }

    /// Whether the `Undo` table claims the inner activity type `kind` — the
    /// [`handles`] question one level deeper, for the withdrawal routes.
    ///
    /// [`handles`]: Self::handles
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles_undo(&self, kind: &str) -> bool {
        self.undo_by_type.contains_key(kind)
    }

    /// Whether the `Create` sub-registry claims `object_type` — the
    /// [`handles_create`](Self::handles_create) question for the create
    /// routes, asked by the handler modules that register them.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles_create(&self, object_type: &str) -> bool {
        self.create_by_object_type.contains_key(object_type)
    }

    /// Whether the registry takes a `Create` whose object is only a
    /// reference — the [`handles_create`](Self::handles_create) question
    /// asked of the `create_by_reference` route.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn takes_create_reference(&self) -> bool {
        self.create_by_reference.is_some()
    }

    /// Whether the `Update` sub-registry claims `object_type` — the
    /// [`handles_create`](Self::handles_create) question for the edit
    /// routes, asked by the handler modules that register them.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles_update(&self, object_type: &str) -> bool {
        self.update_by_object_type.contains_key(object_type)
    }

    /// Whether a `Create` of `object_type` is put to a pre-check before it
    /// reaches the object type's own handler — the [`handles_create`]
    /// question asked of the guarded half of the sub-registry.
    ///
    /// [`handles_create`]: Self::handles_create
    #[cfg(test)]
    #[must_use]
    pub(crate) fn pre_checks_create(&self, object_type: &str) -> bool {
        self.create_prechecks
            .iter()
            .any(|guard| guard.object_type == object_type)
    }

    /// Whether the `Delete` sub-registry claims `object_type` — the
    /// [`handles_create`](Self::handles_create) question one level deeper,
    /// for the handler modules that register both arms of a type.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn handles_delete(&self, object_type: &str) -> bool {
        self.delete_by_object_type.contains_key(object_type)
    }

    /// Sets the fallback the `Delete` route reaches when no entry of the
    /// `delete_by_object_type` table claims the delivered object.
    pub fn register_delete_fallback(&mut self, handler: HandlerFn) {
        self.delete_fallback = Some(handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live registry: `Block`/`Flag`/`Add`/`Remove`/`Move` and
    /// `Like`/`EmojiReact`/`Announce` route by their own names, the follow
    /// handshake's `Follow`/`Accept`/`Reject` join them and the `Undo` table
    /// holds the withdrawals of a `Block`, of the three interactions and of a
    /// `Follow`, the `Update` table is keyed by the five actor object
    /// types plus the note family's editable four, `Delete` runs through
    /// the object-type entries the chat and note handlers claim and the
    /// fallback that resolves an object id, and the `Create` table is keyed
    /// by the eight `Note`-family object types the parser accepts plus the
    /// chat handler's `ChatMessage`, with the poll ticket's guard running
    /// ahead of the `Note` entry it shares with the note handler. The
    /// tables are also the entry points later tickets add their own keys to.
    /// A registry table's keys in order, for the set-equality read of
    /// [`assert_table`].
    fn table_keys<V>(map: &HashMap<&'static str, V>) -> Vec<&'static str> {
        let mut keys = map.keys().copied().collect::<Vec<_>>();
        keys.sort_unstable();
        keys
    }

    /// A registry table holds exactly the space-separated names `want`:
    /// every entry point claimed by one handler and none more — the set
    /// itself is the contract the registered tickets promise.
    fn assert_table<V>(map: &HashMap<&'static str, V>, want: &str, why: &str) {
        let mut want = want.split(' ').collect::<Vec<_>>();
        want.sort_unstable();
        assert_eq!(table_keys(map), want, "{why}");
    }

    #[test]
    fn the_live_registry_holds_the_registered_routes() {
        let handlers = Handlers::default();
        assert_table(
            &handlers.by_type,
            "Accept Add Announce Block EmojiReact Flag Follow Like Move QuoteRequest Reject Remove",
            "the misc five, the interaction trio, the follow handshake's three, \
             the quote request, and nothing further: `Create` and the rest ride the other tables",
        );
        assert_table(
            &handlers.undo_by_type,
            "Announce Block EmojiReact Follow Like",
            "the misc withdrawal, the interaction trio's own, and the follow handshake's",
        );
        assert_table(
            &handlers.update_by_object_type,
            "Application Article Group Note Organization Page Person Question Service",
            "the actor objects plus the note family's editable four",
        );
        assert_table(
            &handlers.delete_by_object_type,
            "Article ChatMessage Note Page Question Tombstone",
            "the chat entry plus the note family's deletable five",
        );
        assert!(
            handlers.delete_fallback.is_some(),
            "the Delete fallback is registered: it is what tells a peer deleting itself from one deleting a post"
        );
        let mut create = crate::federation::note_parser::OBJECT_TYPES.to_vec();
        create.push("ChatMessage");
        assert_table(
            &handlers.create_by_object_type,
            &create.join(" "),
            "the note family plus the chat ticket's key: later tickets (polls) add their own",
        );
        assert!(
            handlers.pre_checks_create("Note"),
            "a Create of a `Note` is put to the poll ticket's guard first: an answer is a vote, not a post"
        );
        assert!(
            !handlers.pre_checks_create("Question"),
            "and the guard covers the note shape alone: a `Question` is a poll, folded by the note handler"
        );
    }
}
