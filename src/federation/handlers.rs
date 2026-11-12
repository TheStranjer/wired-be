//! Per-type activity handlers: the code [`crate::federation::dispatch`]
//! routes an accepted activity into.
//!
//! The dispatcher owns everything that is the same for every activity:
//! normalization, containment, the dedupe ledger, the policy hooks and the
//! routing ([`crate::federation::dispatch`]). This module owns what differs
//! by type: one file per family of activities that speak about the same kind
//! of thing, each registering its routes into the dispatcher's
//! [`Handlers::default`] — [`actor`] for the `Update` and `Delete` of a
//! remote account, with the create, note and edit tickets adding their files
//! beside it.
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
pub mod create;

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
    create::register(&mut handlers);
    handlers
}
