//! # The pin writes
//!
//! `POST …/pin` and `POST …/unpin` are Pleroma's `pin` and `unpin`: a user
//! token ([`AuthUser`]) with `write:accounts` (the scope Pleroma's
//! `StatusController` pipeline names for these two actions, not
//! `write:statuses`), both answering the [`Status`] entity as the caller
//! sees it — `pinned` true and `pleroma.pinned_at` the moment of the write
//! after a pin, both empty after a withdrawal, since the row is re-read for
//! the render and the renderer's batched pins read now names the caller.
//! Behind the shared [`status_for_viewer_or_404`] gate stand Pleroma's three
//! refusals, in its order and with its bodies: a status of somebody else's
//! is 422 `"Someone else's status cannot be pinned"`, a `private`, `direct`
//! or `local` one is 422 `"Non-public status cannot be pinned"` (`public`
//! and `unlisted` pass, as Pleroma's `Visibility.public?/1` admits exactly
//! those two), and a reblog row is 422 `"Reblogs cannot be pinned"` —
//! Pleroma refuses anything but a `Note`, `Article` or `Question` object,
//! which here means a boost has no pin. One more limit is this instance's
//! own: past [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses)
//! the write answers 422 `"You have already pinned the maximum number of
//! statuses"`. A reply may be pinned (Pleroma allows it), anything the
//! caller may not see is the shared 404 before any of these rules run, and
//! re-pinning what is already pinned is the unchanged entity with 200 —
//! Pleroma's `User.add_pinned_object_id/2` skips its limit check for a
//! pin that is already there, and so does this route.
//!
//! `POST …/unpin` keeps Pleroma's two arms: the shared 404 for what the
//! caller may not see, 422 `"Someone else's status cannot be unpinned"` for
//! what they may see but do not own, and — unlike the pin — no visibility
//! or reblog rule, because withdrawing a fact that cannot exist is
//! harmless: [`pinned_status::unpin`] deletes nothing and the answer is the
//! unchanged entity with 200.
//!
//! Neither pin write announces itself to the network except through the
//! `Featured` collection's `Add` and `Remove`, which
//! [`crate::services::federation_out::after_pin_changed`] sends to the
//! pinning account's remote followers — the pair lands in this module's
//! `after_pin_changed` hook.
//!
//! # The thread-mute writes
//!
//! `POST …/mute` and `POST …/unmute` are Pleroma's `mute_conversation` and
//! `unmute_conversation` (`StatusController` over `Pleroma.ThreadMute`,
//! `lib/pleroma/thread_mute.ex`): a user token ([`AuthUser`]) with
//! `write:mutes` (the grant Pleroma's `StatusController` pipeline names for
//! exactly these two actions), both answering the [`Status`] entity as the
//! caller sees it — `muted` true and `pleroma.thread_muted` true after the
//! mute, both false after the withdrawal, since the row is re-read for the
//! render and the renderer's batched thread-mute read now names the caller.
//! The mute is a fact about the status's **thread** (its
//! [`conversation_id`](crate::models::status::Status::conversation_id)),
//! never about the named row alone, and it is stored by
//! [`crate::models::conversation_mute`] — the same table the streaming
//! socket filters on and the notification tickets suppress through.
//!
//! Behind the shared [`status_for_viewer_or_404`] gate stands the
//! participation rule Mastodon requires of these routes: only an account
//! that *is part of* the conversation may mute it — authored a status in it
//! or mentioned in one ([`participates_in`](crate::models::conversation_mute::participates_in),
//! the ticket's two-way query). A caller who is neither answers the same
//! 404 `{"error":"Record not found"}` as an invisible status, so the route
//! never tells a stranger what exists or who posts in a thread. `…/mute`
//! applies the rule; `…/unmute` does not need it — an open thread withdraws
//! to nothing, and [`unmute`](crate::models::conversation_mute::unmute)
//! deletes whatever pair it is given, so both writes are idempotent: a
//! repeat mute is one row and one 200, and a withdrawal that found nothing
//! is the unchanged entity with 200. Neither write federates anything and
//! neither rides a throttle bucket: a thread mute is the account's own
//! private view choice, exactly like a bookmark.
//!
//! [`Status`]: crate::entities::status::Status
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::AuthUser,
    config::Config,
    entities::status::Status,
    error::{AppError, AppResult},
    models::{
        conversation_mute,
        pinned_status::{self, PinnedStatus},
        status::{Status as StatusRow, Visibility},
    },
    oauth::scopes::{WRITE_ACCOUNTS, WRITE_MUTES},
    services::federation_out,
    state::AppState,
};

use super::{NO_REBLOG, NOT_PUBLIC, NOT_YOURS, NOT_YOURS_UNPIN, PIN_LIMIT, current_entity};

/// `POST /api/v1/statuses/{id}/pin` — pin the caller's own status `id` to
/// their profile and answer it as they see it: `pinned` true and
/// `pleroma.pinned_at` the moment of the write, because the row is re-read
/// for the render and the renderer's batched pins read now names the caller.
///
/// The status resolves through [`status_for_viewer_or_404`] like every other
/// route keyed by id — Pleroma's `CommonAPI.pin` asks
/// `activity_visible_to_actor` first and answers its `:visibility_error`
/// arm as a 404 — and [`pin_refusal`] then applies the three rules behind
/// that gate, whose 422s are this route's own bodies. [`at_pin_limit`] is
/// the last of them, weighed only for a pin that is not already there: a
/// restatement is the unchanged entity with 200 even at the limit, which is
/// what Pleroma's `User.add_pinned_object_id/2` does when the object is
/// already in the map.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, for the row a delete took between
/// the gate and the re-read, and for the one row the render cannot answer;
/// [`AppError::Unprocessable`] for one of the four refusals. Any database
/// failure arrives through `?` as [`AppError::Database`].
pub(super) async fn pin_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_ACCOUNTS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if let Some(refusal) = pin_refusal(auth.0.id, &row) {
        return Err(AppError::Unprocessable(refusal.to_owned()));
    }
    let already_pinned = pinned_status::pinned_ids(&state.db, auth.0.id, &[row.id])
        .await?
        .contains_key(&row.id);
    if !already_pinned && at_pin_limit(&state, auth.0.id).await? {
        return Err(AppError::Unprocessable(PIN_LIMIT.to_owned()));
    }
    if let Some(pinned) = pinned_status::pin(&state.db, auth.0.id, row.id).await? {
        debug!(
            status = %row.id,
            user = %auth.0.id,
            "status pinned"
        );
        after_pin_changed(&state, &pinned, true).await;
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/unpin` — withdraw the caller's pin of the
/// status `id` and answer it as they see it: `pinned` false and
/// `pleroma.pinned_at` `null`, since the row is re-read for the render and
/// the renderer's batched pins read no longer names it.
///
/// The same [`status_for_viewer_or_404`] gate, then Pleroma's one remaining
/// rule — a pin is a fact about the *author's* profile, so somebody else's
/// visible status is refused with its own body, [`NOT_YOURS_UNPIN`], the
/// same rule [`pin_refusal`] applies under a different wording. What the
/// unpin does **not** ask is the visibility or the reblog rule: neither can
/// describe a stored pin, and Pleroma's `unpin` runs neither.
/// [`pinned_status::unpin`] is content with nothing to delete, so a pin that
/// was never made (or already withdrawn) changes nothing and is the
/// unchanged [`Status`] with 200.
///
/// # Errors
///
/// Those of [`pin_status`] minus the refusals it does not run:
/// [`AppError::Unprocessable`] only for a status the caller does not own.
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn unpin_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_ACCOUNTS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if row.user_id != auth.0.id {
        return Err(AppError::Unprocessable(NOT_YOURS_UNPIN.to_owned()));
    }
    if let Some(removed) = pinned_status::unpin(&state.db, auth.0.id, row.id).await? {
        debug!(
            status = %row.id,
            user = %auth.0.id,
            "pin withdrawn"
        );
        after_pin_changed(&state, &removed, false).await;
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/mute` — mute the thread the status `id`
/// belongs to and answer that status as the caller sees it: `muted` true
/// and `pleroma.thread_muted` true, because the row is re-read for the
/// render and the renderer's batched thread-mute read now names the caller.
///
/// The status resolves through the shared [`status_for_viewer_or_404`], so
/// a malformed, unknown, deleted or invisible id is the module's one 404 —
/// Pleroma's `mute_conversation` renders its `:not_found` and `:visibility_error`
/// arms alike. Behind that gate stands the participation rule this instance
/// takes from Mastodon: the caller must have authored a status in the
/// conversation or be mentioned in one, and [`participates_in`] answers for
/// both halves in one read. A stranger to the thread gets the same 404 the
/// invisible status gets — the route says nothing about threads the caller
/// is not part of. The write
/// ([`mute`](crate::models::conversation_mute::mute)) is idempotent on
/// `(account, thread)`: a repeat mute is one row, the same flags and one
/// 200. Nothing federates, and the route rides no throttle bucket.
///
/// [`participates_in`]: crate::models::conversation_mute::participates_in
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:mutes` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, for a caller who is not part of
/// the thread, for the row a delete took between the gate and the re-read,
/// and for the one row the render cannot answer. Any database failure
/// arrives through `?` as [`AppError::Database`].
pub(super) async fn mute_conversation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_MUTES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if !conversation_mute::participates_in(&state.db, auth.0.id, row.conversation_id).await? {
        return Err(AppError::NotFound);
    }
    conversation_mute::mute(&state.db, auth.0.id, row.conversation_id).await?;
    debug!(
        conversation = %row.conversation_id,
        user = %auth.0.id,
        "thread muted"
    );
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/unmute` — withdraw the caller's mute of the
/// thread the status `id` belongs to and answer that status as they see it:
/// `muted` false and `pleroma.thread_muted` false, since the row is re-read
/// for the render and the renderer's batched thread-mute read no longer
/// names the thread.
///
/// The same [`status_for_viewer_or_404`] gate as [`mute_conversation`], and
/// the same 404 for everything it hides; the participation rule is not
/// re-asked here — Pleroma's `unmute_conversation` asks only visibility, and
/// withdrawing a fact that cannot exist is harmless either way.
/// [`unmute`](crate::models::conversation_mute::unmute) deletes whatever
/// pair it is given and is content with none, so a mute that was never made
/// (or already withdrawn) changes nothing and is the unchanged [`Status`]
/// with 200.
///
/// # Errors
///
/// Those of [`mute_conversation`] minus the participation refusal: an
/// account with no place in the thread can still withdraw a mute nobody
/// else's call ever wrote. Any database failure arrives through `?` as
/// [`AppError::Database`].
pub(super) async fn unmute_conversation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_MUTES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    conversation_mute::unmute(&state.db, auth.0.id, row.conversation_id).await?;
    debug!(
        conversation = %row.conversation_id,
        user = %auth.0.id,
        "thread unmuted"
    );
    current_entity(&state, &auth.0, row.id).await
}

/// Why `POST …/pin` refuses `row` for the account `caller_id`, in Pleroma's
/// order and with its bodies: ownership first — a pin is a fact about the
/// author's own profile (`activity_belongs_to_actor`) — then the visibility
/// rule, whose `public` and `unlisted` are the only two [`Visibility`]
/// values Pleroma's `Visibility.public?/1` admits, then the shape rule its
/// `object_type_is_allowed_for_pin` serves by admitting a `Note`, an
/// `Article` and a `Question` and no `Announce`, which here means a reblog
/// row. `None` is the pass, and a reply passes: Pleroma pins one.
///
/// Whether `caller_id` may see `row` at all came before this, at
/// [`status_for_viewer_or_404`] — the shared 404 Pleroma's
/// `:visibility_error` arm renders — which is why an invisible post of
/// somebody else's is never one of these 422s.
fn pin_refusal(caller_id: Uuid, row: &StatusRow) -> Option<&'static str> {
    if row.user_id != caller_id {
        return Some(NOT_YOURS);
    }
    if !matches!(row.visibility, Visibility::Public | Visibility::Unlisted) {
        return Some(NOT_PUBLIC);
    }
    if row.reblog_of_id.is_some() {
        return Some(NO_REBLOG);
    }
    None
}

/// Whether `user_id` has already pinned as many statuses as this instance
/// lets them: their [`count`](pinned_status::count) at or above
/// [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses),
/// Pleroma's `[:instance, :max_pinned_statuses]`.
async fn at_pin_limit(state: &AppState, user_id: Uuid) -> AppResult<bool> {
    Ok(pinned_status::count(&state.db, user_id).await? >= pinned_limit(&state.config))
}

/// The configured pin limit as the `i64` the count read answers with. A
/// limit too wide for `i64` is a limit nobody can reach, so it becomes the
/// largest number the comparison can hold rather than a refusal of the
/// configuration.
fn pinned_limit(config: &Config) -> i64 {
    i64::try_from(config.max_pinned_statuses).unwrap_or(i64::MAX)
}

/// Post-write hook of both pin routes: announces the featured collection's
/// `Add` or `Remove` through [`federation_out::after_pin_changed`] —
/// Pleroma's `Builder.pin` and `Builder.unpin`, delivered to the pinning
/// account's remote followers (decision 11 queues the fan-out).
///
/// `pinned` is the row the write just stored or removed: its
/// [`user_id`](PinnedStatus::user_id) names the actor whose collection moves
/// and its [`status_id`](PinnedStatus::status_id) the post to add to or
/// withdraw from it.
///
/// `pinning` states which way the write moved the fact: `true` for the pin a
/// `POST …/pin` left behind — a repeat pin never reaches this hook, since it
/// changed nothing — and `false` for the `Remove` of the pin a
/// `POST …/unpin` withdrew. A withdrawal that found no pin to remove never
/// reaches it either.
///
/// Intentionally the federation half alone: unlike the favourite and boost
/// hooks this one has no notification to send, and the routes ride no
/// throttle bucket.
pub(crate) async fn after_pin_changed(state: &AppState, pinned: &PinnedStatus, pinning: bool) {
    federation_out::after_pin_changed(state, pinned, pinning).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::statuses::test_support::authored;

    #[test]
    fn your_own_public_or_unlisted_post_is_the_one_a_pin_admits() {
        let owner = Uuid::now_v7();
        let own = authored(Uuid::now_v7(), owner);
        assert_eq!(pin_refusal(owner, &own), None, "a public post of yours");
        let mut quiet = authored(Uuid::now_v7(), owner);
        quiet.visibility = Visibility::Unlisted;
        assert_eq!(pin_refusal(owner, &quiet), None, "and an unlisted one");
    }

    #[test]
    fn a_post_that_is_not_public_has_no_pin() {
        let owner = Uuid::now_v7();
        for visibility in [Visibility::Private, Visibility::Direct, Visibility::Local] {
            let mut hidden = authored(Uuid::now_v7(), owner);
            hidden.visibility = visibility;
            assert_eq!(
                pin_refusal(owner, &hidden),
                Some(NOT_PUBLIC),
                "Pleroma's `Visibility.public?/1` admits neither `{visibility}`"
            );
        }
    }

    #[test]
    fn ownership_is_asked_before_anything_else() {
        // Somebody else's *private* status is refused as somebody else's: the
        // visibility rule is the author's own business, and Pleroma's `pin`
        // pipeline asks `activity_belongs_to_actor` first.
        let (owner, caller) = (Uuid::now_v7(), Uuid::now_v7());
        let mut theirs = authored(Uuid::now_v7(), owner);
        theirs.visibility = Visibility::Private;
        assert_eq!(pin_refusal(caller, &theirs), Some(NOT_YOURS));
        theirs.visibility = Visibility::Public;
        assert_eq!(
            pin_refusal(caller, &theirs),
            Some(NOT_YOURS),
            "and a visible post of theirs is still not yours to pin"
        );
    }

    #[test]
    fn a_reblog_has_no_pin_and_a_reply_still_has_one() {
        let owner = Uuid::now_v7();
        let mut boost = authored(Uuid::now_v7(), owner);
        boost.reblog_of_id = Some(Uuid::now_v7());
        assert_eq!(pin_refusal(owner, &boost), Some(NO_REBLOG));

        // The shape rule comes after the visibility rule, and a reply is
        // untouched by either: Pleroma pins one.
        let mut hidden_boost = authored(Uuid::now_v7(), owner);
        hidden_boost.visibility = Visibility::Direct;
        hidden_boost.reblog_of_id = Some(Uuid::now_v7());
        assert_eq!(pin_refusal(owner, &hidden_boost), Some(NOT_PUBLIC));

        let mut answer = authored(Uuid::now_v7(), owner);
        answer.in_reply_to_id = Some(Uuid::now_v7());
        answer.in_reply_to_account_id = Some(Uuid::now_v7());
        assert_eq!(pin_refusal(owner, &answer), None, "a reply is pinnable");
    }

    #[test]
    fn the_pin_limit_reaches_the_count_read_in_its_own_type() {
        let mut config = Config::for_tests();
        assert_eq!(pinned_limit(&config), 1, "the shipped default");
        config.max_pinned_statuses = 5;
        assert_eq!(pinned_limit(&config), 5, "a configured number");
        config.max_pinned_statuses = usize::MAX;
        assert_eq!(
            pinned_limit(&config),
            i64::MAX,
            "a limit too wide for the count's own type is no limit anyone \
             can reach"
        );
    }
}
