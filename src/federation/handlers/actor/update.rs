//! Applying an `Update` of a remote actor: the staleness check, the mirror
//! write and the move fields.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        mrf,
        remote_actor::{ActorError, NewRemoteUser, parse_actor, upsert_remote_user},
    },
    models::user::{self, User},
    state::AppState,
    util::time::parse_time,
};

/// The body of [`update_actor`], in the order the ticket fixes: the shape
/// checks, the staleness check, the two columns the mirror write leaves alone,
/// then the mirror write itself.
pub(super) async fn apply_update(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        return Ok(Outcome::Ignored(
            "an Update whose object is a bare reference",
        ));
    };
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Update of an account this instance owns",
        ));
    }
    if view.object.id() != Some(view.actor.as_str()) {
        debug!(
            object_id = view.object.id().unwrap_or_default(),
            "an Update of a document that is not the sending actor's own; ignoring"
        );
        return Ok(Outcome::Ignored(
            "an Update whose object id is not the actor's own id",
        ));
    }
    if is_stale(object, signer.updated_at) {
        return Ok(Outcome::Ignored(
            "an Update no newer than the stored mirror",
        ));
    }
    let document = mrf::filter_actor(&state, object.clone()).await;
    let parsed = match parse_actor(&document) {
        Ok(parsed) => parsed,
        Err(err) => return refused(err),
    };
    // Compared against the row as it stands before the update: that is what
    // "changed" means here, and the upsert below owns every column it
    // touches, so the two writes cannot disagree.
    store_move_fields(&state.db, &signer, &parsed).await?;
    let updated = match upsert_remote_user(&state.db, parsed).await {
        Ok(stored) => stored,
        Err(err) => return refused(err),
    };
    debug!(account = %updated.nickname, "an Update folded a remote actor's new profile");
    Ok(Outcome::Handled)
}

/// Whether the embedded document is no newer than the stored mirror, the
/// staleness check of [`update_actor`]: `true` only when the document
/// carries a parseable `updated` that is not after `stored_at`. A document
/// with no `updated`, or one that does not parse, says nothing about its
/// age and is applied, so two edits in quick succession both land.
fn is_stale(document: &Value, stored_at: DateTime<Utc>) -> bool {
    document
        .get("updated")
        .and_then(Value::as_str)
        .and_then(parse_time)
        .is_some_and(|updated| updated <= stored_at)
}

/// What a refused actor document means for the activity: a document that is
/// not a usable actor, and a nickname another stored account already owns,
/// are nothing to fold — ignored, with the reason in the debug log. (A
/// stale remote mirror of the same origin holding the nickname is renamed
/// by [`upsert_remote_user`] and never gets here; the refused holder is a
/// local or foreign-domain row.) A database failure is the one arm a retry
/// can fix.
fn refused(err: ActorError) -> Result<Outcome, FederationError> {
    match err {
        ActorError::Failed(err) => Err(FederationError::transient(
            "could not store the remote actor's actor mirror",
            &err,
        )),
        ActorError::Invalid(_) => {
            debug!(%err, "an Update's embedded document is not a usable actor");
            Ok(Outcome::Ignored(
                "an Update whose object is not a valid actor document",
            ))
        }
        ActorError::NicknameConflict => {
            debug!(%err, "an Update's nickname belongs to another stored actor");
            Ok(Outcome::Ignored(
                "an Update whose nickname is owned by a different actor id",
            ))
        }
    }
}

/// Writes the two `users` columns [`upsert_remote_user`] leaves alone — the
/// move target and the alias list — but only the ones whose stored value
/// differs from what the document says, so an `Update` repeating the current
/// profile writes nothing new. A document that drops a field the mirror has
/// stores the clearing: the peer's own document is the authority on its
/// account.
async fn store_move_fields(
    pool: &PgPool,
    stored: &User,
    parsed: &NewRemoteUser,
) -> Result<(), FederationError> {
    if parsed.moved_to != stored.moved_to_ap_id {
        user::set_moved_to(pool, stored.id, parsed.moved_to.as_deref())
            .await
            .map_err(|err| {
                FederationError::transient("could not store the remote actor's move target", &err)
            })?;
    }
    if parsed.also_known_as != stored.also_known_as {
        user::set_also_known_as(pool, stored.id, &parsed.also_known_as)
            .await
            .map_err(|err| {
                FederationError::transient("could not store the remote actor's aliases", &err)
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
