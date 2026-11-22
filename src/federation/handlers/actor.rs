//! Inbound `Update` and `Delete` of an actor: what a peer editing its own
//! profile, or retiring its own account, does to this instance.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.SideEffects`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`): its `Update` clause
//! splits on whether the object the activity carries is an actor and, when
//! it is, rebuilds the stored user from the document; its `Delete` clause
//! resolves the deleted id to an object or to a `User` and, for a user,
//! calls `User.delete` — `perform :delete` in `lib/pleroma/user.ex`, which
//! unfollows every follower and every followed account and deletes the
//! account's activities and the notifications they caused. The ticket rules
//! are authoritative where the two differ:
//!
//! * [`update_actor`], registered for the five actor object types
//!   [`parse_actor`] accepts, folds the embedded document through the same
//!   [`filter_actor`](crate::federation::mrf::filter_actor) and
//!   parse-and-upsert pair the
//!   fetcher uses, so profile, keys, endpoints and fields move together —
//!   plus the two columns that upsert deliberately leaves alone,
//!   `moved_to_ap_id` and `also_known_as`, whenever the document changed
//!   them. It runs only when the object's `id` *is* the activity's `actor`
//!   (an `Update` of somebody else's document is nothing to fold), and not
//!   more often than once per `MIN_UPDATE_INTERVAL` per account, so a peer
//!   stuck redelivering one profile change cannot make this instance rewrite
//!   the mirror forever.
//! * [`delete_actor`] is the `Delete` route for the objects
//!   `delete_by_object_type` does not claim, which is where a bare id has to
//!   be resolved. When it names the sending actor's own account the account
//!   is gone: the mirror is deactivated with `deleted_at` stamped and the
//!   [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)
//!   sweep is queued to take its statuses, relationships and interactions
//!   with it, keeping the row. An id that names anything else is somebody
//!   else's business — the deletion of a status is the status-delete
//!   ticket's entry in that same table — and is ignored.
//!
//! Both handlers run behind the dispatcher's own gates: containment has
//! already proved the activity's `actor` to be the signer's `ap_id`
//! ([`crate::federation::dispatch`]), so the [`User`] a handler is handed
//! *is* the account the activity speaks about.

use chrono::TimeDelta;
use futures::future::BoxFuture;

use self::{delete::apply_delete, update::apply_update};
#[cfg(doc)]
use crate::federation::remote_actor::parse_actor;
use crate::{
    error::AppError,
    federation::dispatch::{ActivityView, FederationError, Handlers, Outcome},
    models::user::User,
    state::AppState,
};

mod delete;
#[cfg(test)]
mod test_support;
mod update;

/// The object types an `Update` of an actor may carry: the same closed set
/// [`parse_actor`] accepts as actor kinds, which is what makes this
/// module's registrations "actor objects only" and leaves
/// `update_by_object_type` free to gain `Note` with the status-edit ticket.
const ACTOR_OBJECT_TYPES: [&str; 5] = ["Person", "Service", "Group", "Organization", "Application"];

/// The flood guard: how long a mirror written by a fetch or by an earlier
/// `Update` answers for itself. Ten seconds — far shorter than the fetcher's
/// own refresh window, so it stops a peer repeating one profile change in a
/// loop rather than delaying a legitimate edit.
const MIN_UPDATE_INTERVAL: TimeDelta = TimeDelta::seconds(10);

/// Folds this module's actor routes into the dispatcher's registry.
///
/// One `Update` entry per object type in the module's actor-kind list — the
/// five kinds `parse_actor` mirrors — and [`delete_actor`] as the `Delete`
/// route for the objects no type-specific entry claims.
///
/// `crate::federation::handlers::registry` calls it, which is the whole
/// composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    for object_type in ACTOR_OBJECT_TYPES {
        handlers.register_update(object_type, update_actor);
    }
    handlers.register_delete_fallback(delete_actor);
}

/// The `Update` handler of an actor object: mirrors the embedded document
/// into the stored account.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// mirror from being written; anything wrong with the *document* is reported
/// as [`Outcome::Ignored`], not as an error.
pub fn update_actor(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_update(state, view, signer).await })
}

/// The `Delete` fallback: the account deletion a peer announces about
/// itself, resolved from the object's id.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// account from being marked deleted or the sweep from being queued.
pub fn delete_actor(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_delete(state, view, signer).await })
}

/// The retry arm every database failure in these handlers maps onto: the
/// writes here are idempotent, so the queue's next attempt can run the same
/// activity again.
fn transient(what: &str, err: &AppError) -> FederationError {
    FederationError::Transient(format!("could not store the remote actor's {what}: {err}"))
}
