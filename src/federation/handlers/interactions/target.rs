//! The shared first step of every interaction fold: the live status the
//! activity's object names, folded to its original and checked for
//! visibility to the actor.

use tracing::debug;

use super::transient;
use crate::{
    api::statuses::action_target_id,
    federation::{
        dispatch::{ActivityView, FederationError},
        fetch_object,
    },
    models::{
        status::{self, Status},
        status_visibility,
        user::User,
    },
    state::AppState,
};

/// The shared step of every interaction fold: the live status the
/// activity's `object` names — resolved through
/// [`fetch_object::resolve_status`] (a local status answers without the
/// network; an unknown remote one is fetched and ingested; a gone or
/// missing object answers nothing) — folded to its original when the row
/// names a reblog, and admitted only while it stands visible to the
/// actor.
///
/// # Errors
///
/// [`FederationError::Transient`] for a network or database failure the
/// retry may outlive; `Ok(None)` is the [`Outcome::Ignored`] answer, after
/// the debug line that says which arm refused.
pub(super) async fn resolve_target(
    state: &AppState,
    view: &ActivityView,
    actor: &User,
) -> Result<Option<Status>, FederationError> {
    let Some(id) = view.object.id() else {
        debug!("an interaction whose embedded object names no id");
        return Ok(None);
    };
    let Some(found) = fetch_object::resolve_status(state, id).await? else {
        debug!(
            object = id,
            "the object of an inbound interaction resolves to no status"
        );
        return Ok(None);
    };
    let Some(target) = interaction_target(state, &found).await? else {
        return Ok(None);
    };
    if !status_visibility::can_view(&state.db, &target, Some(actor))
        .await
        .map_err(|err| transient("the visibility read", &err))?
    {
        debug!(
            status = %target.id,
            actor = %actor.nickname,
            "the target status is not visible to the interaction's actor"
        );
        return Ok(None);
    }
    Ok(Some(target))
}

/// The row the interaction lands on: `found` itself for a post, its live
/// original for a reblog row — the endpoints' `action_target_id` rule, so
/// likes, reactions and boosts of a boost all land on the one post. `None`
/// (after the debug line) is a reblog whose original is gone.
async fn interaction_target(
    state: &AppState,
    found: &Status,
) -> Result<Option<Status>, FederationError> {
    let target_id = action_target_id(found);
    if target_id == found.id {
        return Ok(Some(found.clone()));
    }
    let original = status::get_by_id(&state.db, target_id)
        .await
        .map_err(|err| transient("the original read", &err))?;
    if original.is_none() {
        debug!(
            reblog = %found.id,
            "a boost whose original is no longer stored is nobody's interaction target"
        );
    }
    Ok(original)
}
