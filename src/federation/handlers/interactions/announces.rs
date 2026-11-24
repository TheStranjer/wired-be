//! The `Announce` fold and its withdrawal: boostability, the relay arm and
//! the reblog row's own timestamp.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use super::{NO_INNER_ID, NO_TARGET, UNDO_LOCAL_SIGNER, target::resolve_target};
use crate::{
    api::statuses::after_reblog_changed,
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        json::clamped_time,
        note_parser,
    },
    models::{
        relay,
        status::{self, Status, Visibility},
        user::User,
    },
    state::AppState,
};

/// The body of [`handle_announce`]: the shared target step, the relay arm,
/// the boostability rule, the duplicate read, and the reblog row the
/// activity itself describes.
pub(super) async fn apply_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Announce signed by an account this instance owns",
        ));
    }
    let Some(target) = resolve_target(&state, &view, &signer).await? else {
        return Ok(Outcome::Ignored(NO_TARGET));
    };
    if is_accepted_relay(&state, &signer).await? {
        debug!(
            status = %target.id,
            actor = %signer.nickname,
            "a relay Announce's object was resolved and ingested; no reblog row is folded"
        );
        return Ok(Outcome::Handled);
    }
    if !boostable(target.visibility) {
        debug!(status = %target.id, visibility = target.visibility.as_str(),
               "a boost of a status that is not public or unlisted");
        return Ok(Outcome::Ignored(
            "a boost of a status that is not public or unlisted",
        ));
    }
    if boost_stands(&state.db, &signer, &target, &view.id).await? {
        return Ok(Outcome::Ignored(
            "this actor's boost of that status stands already",
        ));
    }
    let new = status::NewStatus {
        uri: Some(view.id.clone()),
        user_id: signer.id,
        visibility: note_parser::parse_visibility(&view.raw, &signer),
        conversation_id: Some(target.conversation_id),
        reblog_of_id: Some(target.id),
        inserted_at: Some(announced_at(&view.raw, Utc::now())),
        ..Default::default()
    };
    let reblog = status::insert(&state.db, &state.config.base_url, &new)
        .await
        .map_err(|err| {
            FederationError::transient("could not fold the remote interaction's boost", &err)
        })?;
    after_reblog_changed(&state, &reblog, true).await;
    debug!(
        status = %target.id,
        actor = %signer.nickname,
        visibility = reblog.visibility.as_str(),
        "an inbound Announce boosted a status"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_undo_announce`]: the live boost row the undone
/// `Announce` id names, the signer's own, tombstoned through the
/// endpoint's withdrawal write.
pub(super) async fn apply_undo_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(UNDO_LOCAL_SIGNER));
    }
    let Some(announce_id) = view.object.id() else {
        return Ok(Outcome::Ignored(NO_INNER_ID));
    };
    let stored = status::get_by_uri(&state.db, announce_id)
        .await
        .map_err(|err| {
            FederationError::transient("could not fold the remote interaction's boost lookup", &err)
        })?;
    let claim = stored.filter(|row| row.user_id == signer.id && row.deleted_at.is_none());
    let Some(reblog) = claim else {
        debug!(announce_id, "an Undo naming no live boost of its actor");
        return Ok(Outcome::Ignored(
            "an Undo of an Announce its actor never made",
        ));
    };
    let Some(original_id) = reblog.reblog_of_id else {
        return Ok(Outcome::Ignored("an Undo naming a status that is no boost"));
    };
    let removed = status::remove_reblog(&state.db, signer.id, original_id)
        .await
        .map_err(|err| {
            FederationError::transient(
                "could not fold the remote interaction's boost withdrawal",
                &err,
            )
        })?;
    let Some(tombstone) = removed else {
        return Ok(Outcome::Ignored("that boost is already withdrawn"));
    };
    after_reblog_changed(&state, &tombstone, false).await;
    debug!(
        status = %original_id,
        actor = %signer.nickname,
        "an inbound Undo withdrew a boost"
    );
    Ok(Outcome::Handled)
}

/// Whether a status may be boosted: `public` and `unlisted` answers, the
/// ticket's rule restated — and, with it, the relay's re-broadcast of a
/// public post the instance's users never followed anybody for.
const fn boostable(visibility: Visibility) -> bool {
    matches!(visibility, Visibility::Public | Visibility::Unlisted)
}

/// Whether `signer` is a relay this instance follows and that has accepted
/// the follow ([`relay::is_accepted`]): its `Announce`es are the
/// re-broadcasts the subscription exists to receive — the object rides in
/// through [`resolve_target`]'s fetch and stands as an ordinary public
/// remote status, and no reblog row names the relay as its booster. A
/// still-`pending` subscription has not opened that door: its actor folds
/// like any other one until its `Accept` arrives.
async fn is_accepted_relay(state: &AppState, signer: &User) -> Result<bool, FederationError> {
    relay::is_accepted(&state.db, &signer.ap_id)
        .await
        .map_err(|err| {
            FederationError::transient(
                "could not fold the remote interaction's the relay read",
                &err,
            )
        })
}

/// Whether this `Announce` is already folded: the pair holds a live reblog
/// row (one boost per account and status, the table's own rule), or a row
/// already stands under the activity's own `uri` — tombstone included, the
/// id stays reserved (decision 6), so a redelivery of a withdrawn boost
/// restates nothing.
///
/// # Errors
///
/// [`FederationError::Transient`] for either read's own database failure.
async fn boost_stands(
    db: &PgPool,
    signer: &User,
    target: &Status,
    announce_id: &str,
) -> Result<bool, FederationError> {
    let pair = status::reblogged_status_ids(db, signer.id, std::slice::from_ref(&target.id))
        .await
        .map_err(|err| {
            FederationError::transient(
                "could not fold the remote interaction's the boost read",
                &err,
            )
        })?;
    if pair.contains(&target.id) {
        return Ok(true);
    }
    let same_id = status::get_by_uri(db, announce_id).await.map_err(|err| {
        FederationError::transient(
            "could not fold the remote interaction's the boost uri read",
            &err,
        )
    })?;
    Ok(same_id.is_some())
}

/// The reblog row's creation instant: the `Announce` activity's own
/// `published` through [`clamped_time`], which answers `now` when it leads
/// the local clock past [`CLOCK_SKEW`](crate::federation::json::CLOCK_SKEW)
/// — a peer's runaway clock moves nobody's timeline — and so does this
/// function when the field is absent or unreadable.
fn announced_at(activity: &Value, now: DateTime<Utc>) -> DateTime<Utc> {
    clamped_time(activity.get("published"), now).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_boost_takes_the_activities_published_time_on_skew_clamp_and_default() {
        let exact = announced_at(
            &json!({"published": "2024-06-01T10:00:00.000Z"}),
            DateTime::parse_from_rfc3339("2024-06-01T12:00:00Z")
                .expect("the fixture instant parses")
                .with_timezone(&Utc),
        );
        assert_eq!(
            exact.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2024-06-01T10:00:00.000Z",
            "a past published lands verbatim"
        );
        let now = Utc::now();
        let future = announced_at(&json!({"published": "2999-01-01T00:00:00.000Z"}), now);
        assert_eq!(future, now, "a runaway clock is clamped to now");
        for unreadable in [
            json!({}),
            json!({"published": "yesterday"}),
            json!({"published": 7}),
        ] {
            assert_eq!(
                announced_at(&unreadable, now),
                now,
                "{unreadable} defaults to now"
            );
        }
    }

    #[test]
    fn only_a_public_or_unlisted_status_is_boostable() {
        assert!(boostable(Visibility::Public) && boostable(Visibility::Unlisted));
        for private in [Visibility::Private, Visibility::Direct, Visibility::Local] {
            assert!(
                !boostable(private),
                "a boost of a {private} status is ignored"
            );
        }
    }
}
