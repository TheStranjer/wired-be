//! Inbound `Update` and `Delete` of the `Note` family: what a peer editing
//! or withdrawing one of its own posts does to this instance.
//!
//! The reference is Pleroma's `Update` and `Delete` arms of
//! `SideEffects.handle` (`lib/pleroma/web/activity_pub/side_effects.ex`)
//! with the `UpdateValidator`
//! (`lib/pleroma/web/activity_pub/object_validators/update_validator.ex`)
//! as the gate: an `Update` is only an edit when its author is the actor
//! who sent it and its `updated` time postdates what this instance already
//! holds, and a `Delete` retires the object the sender owns — of a post
//! never seen, Pleroma's `Object.create_tombstone` leaves the withdrawn
//! shell behind so nothing re-imports it. The ticket rules are authoritative
//! where the two differ:
//!
//! * [`update_note`], the `update_by_object_type` entry for `Note`,
//!   `Question`, `Article` and `Page`, gates the embedded document on
//!   `attributedTo` naming the activity's `actor` (containment, shared with
//!   [`super::create`]) and looks for a *live* stored status under the
//!   object's `uri` authored by that account. What it finds, it edits; what
//!   it does not — no row at all, or a tombstone — it hands to
//!   [`ingest::ingest_note`], so an `Update` for a post this instance never
//!   received lands the post (an edit of a deleted one is refused by the
//!   ingest's own tombstone check, and its answer rides out unchanged). The
//!   edit itself: a document no newer than the stored version — its
//!   `updated` at or before the row's `edited_at`, or its creation time
//!   while the row has never been edited — is ignored; everything else
//!   re-folds the status through [`crate::models::status_edit`], the same
//!   write the edit endpoint runs, which snapshots the superseded version
//!   into `status_edits` (visible at
//!   `GET /api/v1/statuses/{id}/history`) and stamps `edited_at`. Content,
//!   source text, warning, sensitive flag, hashtags and mentions — resolved
//!   through the create path's own resolver (`ingest::resolve_mentions`) —
//!   are its new state; the attachments are replaced wholesale in the
//!   create path's row shape, through its media writer
//!   (`ingest::insert_media`), since
//!   nobody uploaded them. A `Question`'s update additionally restates its
//!   poll: the option counts, voter count and deadline the remote reports
//!   overwrite the mirrored ones through [`poll::record_remote_totals`] (and
//!   a `Question` that arrived without a poll grows one through the create
//!   path's poll writer), and the edit route's `after_status_edited`
//!   hook — the boosters' telling, and the `Update` a local post would
//!   federate (a mirrored row is its origin's to announce, so that half of
//!   the hook stays silent here) — runs from here exactly as it runs from
//!   there.
//! * [`delete_note`], the `delete_by_object_type` entry for those four types
//!   plus `Tombstone` and the registry's `Delete` fallback, resolves the
//!   object's id — embedded document, bare string reference, either; a
//!   fallback whose id *is* the sender's own actor id goes straight back to
//!   [`super::actor`]'s handler, which owns account withdrawals. Otherwise
//!   the sender's live status with that `uri` goes through
//!   [`crate::services::delete_status`], the one deletion the status
//!   endpoint, the expiry job and this fold share: counters step down,
//!   notifications about the post clear, the tombstone stays. A `Delete`
//!   naming a status the sender does not own is ignored — one account does
//!   not withdraw another's post — and one for an object never stored leaves
//!   [`status::insert_tombstone`]'s shell in its place, so the
//!   [`Create`](super::create) of a deleted-before-arrival post is swallowed
//!   by the ingest's tombstone check.
//!
//! A `Delete` of a bare id whose type this module does not claim still
//! reaches the fallback and is folded here as a status withdrawal or a
//! tombstone — the same call Pleroma makes, resolving the id without
//! consulting its type. Reblog rows an `Undo` of an `Announce` withdraws are
//! the announce ticket's, not this module's; nothing here sends anything
//! anywhere.

use futures::future::BoxFuture;

mod delete;
mod media_poll;
#[cfg(test)]
mod test_support;
mod update;

use self::{delete::apply_delete, update::apply_update};
use crate::{
    federation::dispatch::{ActivityView, FederationError, Handlers, Outcome},
    models::user::User,
    state::AppState,
};
#[cfg(doc)]
use crate::{
    models::{poll, status, status_edit},
    services::{delete_status, ingest},
};

/// The object types whose `Update` this module folds: the four members of
/// the note family that are posts, and so editable ones. The family's
/// media shapes (`Image`, `Video`, `Audio`, `Document`) arrive as statuses
/// through [`super::create`] but carry no text an edit rewrites — an
/// `Update` of one routes nowhere, exactly as Pleroma's side effects leave
/// it.
const EDITED_OBJECT_TYPES: [&str; 4] = ["Note", "Question", "Article", "Page"];

/// The object types whose `Delete` this module folds: the editable four,
/// plus the `Tombstone` document Pleroma and Mastodon sometimes deliver
/// embedded in the `Delete` in place of a bare id.
const DELETED_OBJECT_TYPES: [&str; 5] = ["Note", "Question", "Article", "Page", "Tombstone"];

/// Folds this module's routes into the dispatcher's registry.
///
/// The `update_by_object_type` entries name the four editable types, the
/// `delete_by_object_type` entries those four plus the embedded
/// `Tombstone`, and the `Delete` fallback resolves the ids no embedded type
/// claimed.
///
/// `crate::federation::handlers::registry` calls it last, which is what
/// lets the fallback wrap the actor handler's — see its module doc for the
/// order's meaning.
pub fn register(handlers: &mut Handlers) {
    for object_type in EDITED_OBJECT_TYPES {
        handlers.register_update(object_type, update_note);
    }
    for object_type in DELETED_OBJECT_TYPES {
        handlers.register_delete(object_type, delete_note);
    }
    handlers.register_delete_fallback(delete_note);
}

/// The `Update` handler of a note-family object: the author's own edit,
/// folded — or, for a post this instance never stored, its first landing.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure the retry may
/// outlive; [`FederationError::Permanent`] (the ingest's own) for a document
/// no edit and no create can parse. Everything wrong with the *shape* — the
/// gate's refusals — arrives as [`Outcome::Ignored`].
pub fn update_note(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_update(state, view, signer).await })
}

/// The `Delete` handler of a note-family object, embedded `Tombstone`, and
/// of every `Delete` no other entry claimed: the sender's own post
/// withdrawn, or its tombstone reserved.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure; the fold's two
/// shapes — [`delete_status::delete_status`]'s idempotent soft delete and
/// the `ON CONFLICT DO NOTHING` tombstone stub — both finish what a retry
/// restarts.
pub fn delete_note(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_delete(state, view, signer).await })
}

/// A remote count narrowed to the `integer` column's width — the ingest's
/// same clamping rule: beyond it the column's maximum, never a refused
/// update over a number nobody can vote into being true.
fn whole(count: i64) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_claims_the_note_family_and_the_delete_fallback() {
        let mut handlers = Handlers::new();
        register(&mut handlers);
        for object_type in EDITED_OBJECT_TYPES {
            assert!(
                handlers.handles_update(object_type),
                "an Update of a {object_type} folds here"
            );
            assert!(
                handlers.handles_delete(object_type),
                "a Delete of a {object_type} withdraws here"
            );
        }
        assert!(
            handlers.handles_delete("Tombstone"),
            "and a Delete carrying its embedded Tombstone"
        );
        assert!(
            !handlers.handles_update("Person"),
            "an actor stays the actor module's Update key"
        );
    }
}
