//! Inbound `Follow`, `Accept`, `Reject` and `Undo` of a `Follow`: the
//! follow handshake across the federation boundary.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.SideEffects`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`) — whose `Follow` clause
//! runs `User.follow` and, when the followed local account can answer,
//! builds and pipelines an `Accept` on the spot (a `Reject` when the follow
//! write is refused), whose `Accept` and `Reject` clauses resolve our own
//! outgoing follow by its activity id and update or remove the
//! relationship, and whose `Undo` clause takes back what the undone
//! activity did — beside the answer shapes of
//! `Pleroma.Web.ActivityPub.Builder.accept/2` and `.reject/2`
//! (`lib/pleroma/web/activity_pub/builder.ex`). The ticket rules are
//! authoritative where the two differ:
//!
//! * [`handle_follow`] folds a remote account's follow of a local one: only
//!   a stored **active local** object can be followed (anything else is
//!   ignored), a pair blocked either way ([`block::blocked_between`]) is
//!   answered with a [`reject_follow`] published back and *nothing stored*,
//!   as is a follow from a domain the target blocks
//!   ([`domain_block::is_domain_blocked`]) or of an account that has moved
//!   (Mastodon's `follow.rb` refusal), and every other delivery runs [`follow::follow`] with the delivered
//!   activity id landed on the row as its `ap_id` (decision 6). The
//!   followee's own telling ([`notify::on_follow`]) rides every stored
//!   fold: its type is the relationship's state. An accepted answer — this
//!   instance owns the followee and they are unlocked — sends the
//!   [`accept_follow`] back immediately; a pending one — a locked target —
//!   waits for the endpoint's authorization and sends nothing. A duplicate
//!   Follow restates the standing pair, moves the stored id to the newest
//!   activity, and re-sends the `Accept` when the pair stands accepted.
//! * [`handle_accept`] and [`handle_reject`] answer a follow *we made*: the
//!   relationship is resolved by activity id (`follows.ap_id`) or by the
//!   pair the embedded inner `Follow` names, and only the followee's own
//!   answer folds — a wrong actor, or an unknown follow, is ignored. The
//!   grant runs [`follow::accept`] (`pending` to `accepted`, counters with
//!   it); the refusal runs [`follow::reject`], which removes the row and —
//!   for an accepted relationship the remote revokes — its counters too.
//! * [`handle_undo_follow`] is the `undo_by_type["Follow"]` route: the
//!   inner `Follow`'s actor must be the `Undo`'s own (the dispatcher gates
//!   embedded deliveries; the handler restates the check as its belt), the
//!   row must belong to the signer as its follower, and [`follow::unfollow`]
//!   takes it back with its counters while the pair's `follow` and
//!   `follow_request` notifications go with it.
//!
//! Every route runs behind the dispatcher's own gates: containment has
//! already proved the activity's `actor` to be the signer's `ap_id`
//! ([`crate::federation::dispatch`]), so the [`User`] a handler is handed
//! *is* the remote account the activity speaks for; the local-signer guard
//! each one keeps is the belt to that bracer, as in
//! [`crate::federation::handlers::misc`].
//!
//! [`reject_follow`]: crate::services::federation_out::reject_follow
//! [`accept_follow`]: crate::services::federation_out::accept_follow

use futures::future::BoxFuture;
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        json,
    },
    models::{
        block, domain_block,
        follow::{self, Follow, FollowState},
        notification::{self, TYPE_FOLLOW, TYPE_FOLLOW_REQUEST},
        relay,
        user::{self, User},
    },
    services::{
        federation_out::{self, FollowAnswer},
        notify,
    },
    state::AppState,
};

/// Folds this module's follow-handshake routes into the dispatcher's
/// registry.
///
/// The three types the handshake speaks by their own names — `Follow`,
/// `Accept`, `Reject` — plus the withdrawal of a `Follow` in the `Undo`
/// table. `crate::federation::handlers::registry` calls it, which is the
/// whole composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register("Follow", handle_follow);
    handlers.register("Accept", handle_accept);
    handlers.register("Reject", handle_reject);
    handlers.register_undo("Follow", handle_undo_follow);
}

/// The `Follow` route: fold a remote account's follow of a local one.
///
/// Grant it with an `Accept` back when this instance owns the answer, hold
/// it `pending` with the followee's telling when the target is locked, and
/// answer a blocked pair with a `Reject` and nothing stored.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// target read, the relationship write, its activity id, or the answer's
/// fan-out; a `Follow` naming no active local account is
/// [`Outcome::Ignored`].
pub fn handle_follow(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_follow(state, view, signer).await })
}

/// The `Accept` route: grant the follow this instance made of the signer,
/// resolved by activity id or by the embedded inner `Follow`'s pair.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// lookup or the transition; an unknown follow, or one the signer is not
/// the followee of, is [`Outcome::Ignored`].
pub fn handle_accept(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_accept(state, view, signer).await })
}

/// The `Reject` route: remove the follow this instance made of the signer
/// — a `pending` request the remote will not answer, or an accepted
/// relationship they revoke (its counters step back down with it).
///
/// # Errors
///
/// Those of [`handle_accept`].
pub fn handle_reject(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_reject(state, view, signer).await })
}

/// The `undo_by_type["Follow"]` route: the signer takes back their own
/// follow of a local account — the row with its counters, and the pair's
/// `follow` and `follow_request` tellings.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// lookup or the withdrawal; anything whose `Undo` resolves to no follow of
/// its actor is [`Outcome::Ignored`].
pub fn handle_undo_follow(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_follow(state, view, signer).await })
}

/// The body of [`handle_follow`]: the local-target gate, the blocked pair's
/// `Reject`, the relationship write with its mirror id, and the state's
/// answer — [`accept_follow`] for a granted follow, silence and a telling
/// for a waiting one.
async fn apply_follow(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Follow signed by an account this instance owns",
        ));
    }
    let Some(object_id) = view.object.id() else {
        return Ok(Outcome::Ignored("a Follow whose object names no account"));
    };
    let stored = user::get_by_ap_id(&state.db, object_id)
        .await
        .map_err(|err| transient("follow target", &err))?;
    let Some(target) = stored.filter(User::is_local_active) else {
        return Ok(Outcome::Ignored(
            "a Follow whose object is not a stored active local account",
        ));
    };
    let answer = FollowAnswer {
        follow_id: &view.id,
        follower_ap_id: &signer.ap_id,
    };
    if block::blocked_between(&state.db, signer.id, target.id)
        .await
        .map_err(|err| transient("block check", &err))?
    {
        let _deliveries = federation_out::reject_follow(&state, &target, &answer)
            .await
            .map_err(|err| transient("follow reject", &err))?;
        debug!(
            follower = %signer.nickname,
            followee = %target.nickname,
            "an inbound Follow met a blocked pair and was answered with a Reject"
        );
        return Ok(Outcome::Handled);
    }
    let blocked_domain = match json::http_host(&signer.ap_id) {
        Some(host) => domain_block::is_domain_blocked(&state.db, target.id, &host)
            .await
            .map_err(|err| transient("domain block check", &err))?,
        None => false,
    };
    if blocked_domain || target.moved_to_ap_id.is_some() {
        let _deliveries = federation_out::reject_follow(&state, &target, &answer)
            .await
            .map_err(|err| transient("follow reject", &err))?;
        debug!(
            follower = %signer.nickname,
            followee = %target.nickname,
            blocked_domain,
            "an inbound Follow met a domain block or a moved target and was answered with a Reject"
        );
        return Ok(Outcome::Handled);
    }
    let row = follow::follow(&state.db, &state.config.base_url, &signer, &target)
        .await
        .map_err(|err| transient("follow", &err))?;
    if row.ap_id.as_deref() != Some(view.id.as_str()) {
        follow::set_ap_id(&state.db, row.id, &view.id)
            .await
            .map_err(|err| transient("follow activity id", &err))?;
    }
    notify::on_follow(&state, &signer, &target, Some(&row), true).await;
    let granted = row.state == FollowState::Accepted;
    if granted {
        let _deliveries = federation_out::accept_follow(&state, &target, &answer)
            .await
            .map_err(|err| transient("follow accept", &err))?;
    }
    debug!(
        follower = %signer.nickname,
        followee = %target.nickname,
        granted,
        "an inbound Follow folded a remote account's relationship with a local one"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_accept`]: resolve our own follow, honour only the
/// followee's answer, and grant it through the model's transition.
async fn apply_accept(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Accept signed by an account this instance owns",
        ));
    }
    let row = answered_follow(&state.db, &view)
        .await
        .map_err(|err| transient("follow lookup", &err))?;
    let Some(row) = row else {
        return Ok(Outcome::Ignored(
            "an Accept naming no follow this instance stores",
        ));
    };
    if row.followee_id != signer.id {
        return Ok(Outcome::Ignored(
            "an Accept from an account that is not the followed one",
        ));
    }
    let _granted = follow::accept(&state.db, row.id)
        .await
        .map_err(|err| transient("follow acceptance", &err))?;
    // The relay arm (see `crate::services::relay`): when the granted follow
    // is the instance actor's own follow of a relay, the subscription turns
    // accepted, opening the relay's inbox to the publisher's fan-out. A
    // local user's follow of the relay actor is no subscription and writes
    // nothing here.
    if follower_is_instance_actor(&state.db, row.follower_id)
        .await
        .map_err(|err| transient("instance actor lookup", &err))?
    {
        let relay_granted = relay::mark_accepted(&state.db, &signer.ap_id)
            .await
            .map_err(|err| transient("relay acceptance", &err))?;
        if relay_granted {
            debug!(
                relay = %signer.ap_id,
                "an inbound Accept granted a relay follow: the subscription stands accepted"
            );
        }
    }
    debug!(
        follower = %row.follower_id,
        followee = %signer.nickname,
        "an inbound Accept granted a follow this instance made"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_reject`]: the same resolution and the same
/// followee honour, then [`follow::reject`] — the `pending` request is
/// withdrawn, an accepted relationship the remote revokes goes with its
/// counters.
async fn apply_reject(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Reject signed by an account this instance owns",
        ));
    }
    let row = answered_follow(&state.db, &view)
        .await
        .map_err(|err| transient("follow lookup", &err))?;
    let Some(row) = row else {
        return Ok(Outcome::Ignored(
            "a Reject naming no follow this instance stores",
        ));
    };
    if row.followee_id != signer.id {
        return Ok(Outcome::Ignored(
            "a Reject from an account that is not the followed one",
        ));
    }
    let _removed = follow::reject(&state.db, row.id)
        .await
        .map_err(|err| transient("follow rejection", &err))?;
    // The relay arm: a relay revokes (or refuses) a subscription with a
    // `Reject` of the instance actor's follow. The `follows` row is gone, so
    // the subscription goes back to pending and the relay's inbox leaves
    // the fan-out. A local user's follow of the relay actor changes nothing.
    if follower_is_instance_actor(&state.db, row.follower_id)
        .await
        .map_err(|err| transient("instance actor lookup", &err))?
    {
        let relay_revoked = relay::mark_pending(&state.db, &signer.ap_id)
            .await
            .map_err(|err| transient("relay revocation", &err))?;
        if relay_revoked {
            debug!(
                relay = %signer.ap_id,
                "an inbound Reject revoked a relay follow: the subscription is pending again"
            );
        }
    }
    debug!(
        follower = %row.follower_id,
        followee = %signer.nickname,
        "an inbound Reject removed a follow this instance made"
    );
    Ok(Outcome::Handled)
}

/// Whether `follower_id` is the instance actor — the account whose follows
/// are relay subscriptions. A missing instance actor (never provisioned)
/// means no relay follow can exist, so it answers `false` without creating
/// one.
async fn follower_is_instance_actor(pool: &PgPool, follower_id: Uuid) -> AppResult<bool> {
    let instance = user::get_by_nickname(pool, user::INSTANCE_ACTOR_NICKNAME).await?;
    Ok(instance.is_some_and(|actor| actor.id == follower_id))
}

/// The body of [`handle_undo_follow`]: the inner actor's belt, the signer's
/// own follow under the delivered id or pair, the model's withdrawal with
/// its counters, and the pair's tellings taken back with it.
async fn apply_undo_follow(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Undo of a Follow signed by an account this instance owns",
        ));
    }
    if !inner_follow_is_the_signers(&view, &signer.ap_id) {
        return Ok(Outcome::Ignored(
            "an Undo of a Follow whose actor is somebody else's",
        ));
    }
    let row = answered_follow(&state.db, &view)
        .await
        .map_err(|err| transient("follow lookup", &err))?;
    let Some(row) = row else {
        return Ok(Outcome::Ignored(
            "an Undo naming no follow this instance stores",
        ));
    };
    if row.follower_id != signer.id {
        return Ok(Outcome::Ignored("an Undo of a Follow its actor never made"));
    }
    let Some(target) = user::get_by_id(&state.db, row.followee_id)
        .await
        .map_err(|err| transient("followee read", &err))?
    else {
        return Ok(Outcome::Ignored(
            "an Undo of a Follow whose followee is gone",
        ));
    };
    let _withdrawn = follow::unfollow(&state.db, &signer, &target)
        .await
        .map_err(|err| transient("follow withdrawal", &err))?;
    let _untold = forget_follow_tellings(&state.db, &target, &signer)
        .await
        .map_err(|err| transient("follow tellings cleanup", &err))?;
    debug!(
        follower = %signer.nickname,
        followee = %target.nickname,
        "an inbound Undo withdrew a remote account's follow"
    );
    Ok(Outcome::Handled)
}

/// The relationship an inbound answer or withdrawal refers to: the
/// embedded inner `Follow` names its pair by `actor` and `object`
/// ([`follow::get`] reads it once both resolve to stored accounts), and the
/// id the object carries — an embedded document's own or the bare
/// reference — keys [`follow::find_by_ap_id`]. `None` is a delivery
/// referring to no follow this instance stores.
async fn answered_follow(pool: &PgPool, view: &ActivityView) -> AppResult<Option<Follow>> {
    if let Some(row) = embedded_pair_follow(pool, view).await? {
        return Ok(Some(row));
    }
    match view.object.id() {
        Some(id) => follow::find_by_ap_id(pool, id).await,
        None => Ok(None),
    }
}

/// The relationship the embedded inner activity's `actor` and `object`
/// name: both must reference accounts this instance stores, and that pair
/// must have a follow row — the pair form of [`answered_follow`]'s
/// resolution, for the peers that answer with the whole `Follow` inside.
async fn embedded_pair_follow(pool: &PgPool, view: &ActivityView) -> AppResult<Option<Follow>> {
    let Some(inner) = view.object.embedded() else {
        return Ok(None);
    };
    let (Some(follower_ap_id), Some(target_ap_id)) = (
        inner.get("actor").and_then(json::value_or_id),
        inner.get("object").and_then(json::value_or_id),
    ) else {
        return Ok(None);
    };
    let Some(follower) = user::get_by_ap_id(pool, &follower_ap_id).await? else {
        return Ok(None);
    };
    let Some(target) = user::get_by_ap_id(pool, &target_ap_id).await? else {
        return Ok(None);
    };
    follow::get(pool, follower.id, target.id).await
}

/// Whether the `Undo`'s embedded inner `Follow` is the signer's own: an
/// inner that names an `actor` must name the undoing account's `ap_id` —
/// the dispatcher's gate for embedded deliveries, restated as the handler's
/// belt. An inner that names no actor is already the dispatcher's fallback
/// (it read the signer's own id onto it), and a bare-id delivery has no
/// inner to check.
fn inner_follow_is_the_signers(view: &ActivityView, signer_ap_id: &str) -> bool {
    view.object
        .embedded()
        .and_then(|inner| inner.get("actor"))
        .and_then(json::value_or_id)
        .is_none_or(|actor| actor == signer_ap_id)
}

/// The pair's `follow` and `follow_request` tellings, taken back with the
/// relationship: the withdrawn follow leaves no live telling of itself
/// (Pleroma's `Notification.dismiss` beside its undo clause). Returns how
/// many rows went.
async fn forget_follow_tellings(pool: &PgPool, target: &User, follower: &User) -> AppResult<u64> {
    let granted =
        notification::delete_matching(pool, target.id, follower.id, TYPE_FOLLOW, None).await?;
    let requested =
        notification::delete_matching(pool, target.id, follower.id, TYPE_FOLLOW_REQUEST, None)
            .await?;
    Ok(granted + requested)
}

/// The retry arm every database failure in these handlers maps onto: the
/// writes here are idempotent, so the queue's next attempt can run the
/// same activity again.
fn transient(what: &str, err: &AppError) -> FederationError {
    FederationError::Transient(format!(
        "could not fold the remote follow activity's {what}: {err}"
    ))
}

#[cfg(test)]
mod tests;
