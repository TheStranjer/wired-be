//! Per-type activity handlers: the code [`crate::federation::dispatch`]
//! routes an accepted activity into.
//!
//! The dispatcher owns everything that is the same for every activity:
//! normalization, containment, the dedupe ledger, the policy hooks and the
//! routing ([`crate::federation::dispatch`]). This module owns what differs
//! by type: one file per family of activities that speak about the same kind
//! of thing, each registering its routes into the dispatcher's
//! [`Handlers::default`] — [`actor`] for the `Update` and `Delete` of a
//! remote account, [`misc`] for the `Block` (+`Undo`), `Flag`, `Add`,
//! `Remove` and `Move` that ride beside them, [`interactions`] for the
//! `Like`, `EmojiReact` and `Announce` (and their `Undo`s) that land on a
//! status, with the create and note tickets adding their files in turn.
//!
//! A handler is a plain
//! [`HandlerFn`](crate::federation::dispatch::HandlerFn): it gets the shared
//! state, the normalized
//! [`ActivityView`](crate::federation::dispatch::ActivityView) and the
//! [`User`](crate::models::user::User) whose key authenticated the delivery
//! (whose `ap_id` the dispatcher's containment has already proved is the
//! activity's `actor`), and reports
//! [`Outcome::Handled`](crate::federation::dispatch::Outcome::Handled),
//! [`Outcome::Ignored`](crate::federation::dispatch::Outcome::Ignored) with a
//! static reason, or a
//! [`FederationError`](crate::federation::dispatch::FederationError) the
//! dispatcher maps onto the job — a transient one releasing the ledger row
//! for a retry, anything else discarding it.

pub mod actor;
pub mod chat;
pub mod create;
pub mod interactions;
pub mod misc;
pub mod note;
pub mod vote;

use crate::federation::dispatch::Handlers;

/// The process-wide registry
/// [`handle_incoming`](crate::federation::dispatch::handle_incoming) routes
/// through: every activity type this instance knows how to fold.
///
/// One line per handler module, which is the whole composition: the
/// dispatcher's own [`Handlers::default`] *is* this function, so a ticket
/// that teaches the instance a new type adds its routes to its own file, and
/// its module's name to this one.
#[must_use]
pub fn registry() -> Handlers {
    let mut handlers = Handlers::new();
    actor::register(&mut handlers);
    misc::register(&mut handlers);
    interactions::register(&mut handlers);
    create::register(&mut handlers);
    chat::register(&mut handlers);
    vote::register(&mut handlers);
    // Last, because its `Delete` fallback answers for every object the
    // earlier `register_delete_fallback` calls left unrouted: it hands the
    // actor's own id back to the actor handler and folds the rest.
    note::register(&mut handlers);
    handlers
}
