//! The status endpoints: create, the reads, the state changes and listings,
//! translation and the delete, all over one shared status gate
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The routes are the write `POST /api/v1/statuses`, the lookups
//! `GET /api/v1/statuses/{id}` and the batch
//! `GET /api/v1/statuses?id[]=a&id[]=b`, the thread read
//! `GET /api/v1/statuses/{id}/context`, the card read
//! `GET /api/v1/statuses/{id}/card`, the quoting listing
//! `GET /api/v1/statuses/{id}/quotes` and its Pleroma twin
//! `GET /api/v1/pleroma/statuses/{id}/quotes`, the favourite writes
//! `POST /api/v1/statuses/{id}/favourite` and its `…/unfavourite` twin,
//! the state listing `GET /api/v1/statuses/{id}/favourited_by`, the
//! caller's own favourites list `GET /api/v1/favourites`, the reblog
//! writes `POST /api/v1/statuses/{id}/reblog` and its `…/unreblog` twin,
//! the state listing `GET /api/v1/statuses/{id}/reblogged_by`, the bookmark
//! writes `POST /api/v1/statuses/{id}/bookmark` and its `…/unbookmark`
//! twin, the caller's own bookmarks list `GET /api/v1/bookmarks`, the pin
//! writes `POST /api/v1/statuses/{id}/pin` and its `…/unpin` twin, the
//! thread-mute writes `POST /api/v1/statuses/{id}/mute` and its
//! `…/unmute` twin,
//! `POST /api/v1/statuses/{id}/translate`, the write
//! `DELETE /api/v1/statuses/{id}`, the edit write
//! `PUT /api/v1/statuses/{id}`, the version listing
//! `GET /api/v1/statuses/{id}/history` and the source read
//! `GET /api/v1/statuses/{id}/source`.
//!
//! The reference is Pleroma's `StatusController` (`lib/pleroma/web/
//! mastodon_api/controllers/status_controller.ex`): `create` for the new
//! post, `show` and `.index` for the reads, `favourite` / `unfavourite` /
//! `favourited_by` / `favourites` for the likes, `reblog` / `unreblog` /
//! `reblogged_by` for the boosts, `bookmark` / `unbookmark` / `bookmarks`
//! for the bookmarks, `mute_conversation` / `unmute_conversation` for the
//! thread mutes, and `delete` — with `Pleroma.Web.CommonAPI.delete/2`
//! (`lib/pleroma/web/common_api.ex`, whose `repeat` / `unrepeat` stand
//! behind the boosts) — for the writes. Those, `create` apart, reduce to
//! the same two moves per surface: find the status, ask
//! `Visibility.visible_for_user?`, and either render it or answer
//! `{:error, :not_found}`. Here finding and asking live in the shared
//! [`status_for_viewer_or_404`], which every
//! `/api/v1/statuses/{id}/...` route reuses, and rendering lives in the
//! batched page renderer [`crate::services::render_status`].
//!
//! This module is a tree. `create` holds the create write with its
//! idempotency and schedule arms, `show` the single-status, batch and
//! card reads, `delete_edit` the delete and the edit with the edit
//! hook, `history` the version and source reads, `context` the thread
//! read and its walks, `favourites` the like writes, the favourites
//! listings and the favourite hook, `reblogs` the boost writes, the
//! boosters and quoters listings and the reblog hook, `bookmarks` the
//! bookmark writes and the bookmarks listing, `pins_mutes` the pin and
//! thread-mute writes with the pin hook, and `translate` the
//! translation; this root keeps the routers, the shared refusal bodies
//! and the helpers every child reads through. `test_support` holds the
//! unit-test fixtures the children share.
//!
//! What the reads owe no 422: this module answers an unusable id with the
//! show route's 404 and the batch route's silent skip, and the only
//! request shapes beyond those are the pagination cursors' 400. The create
//! and the edit route do owe them: every refusal
//! [`crate::services::create_status`] documents is their 422 — plus the
//! edit's own `"Cannot edit a reblog"` — on top of the extractor's 401 and
//! the scope gate's 403.
//!
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`soft_delete_with_reblogs`]: crate::models::status::soft_delete_with_reblogs
//! [`filter_visible`]: crate::models::status_visibility::filter_visible
//! [`Status`]: crate::entities::status::Status
//! [`ScheduledStatus`]: crate::entities::scheduled_status::ScheduledStatus
//! [`Account`]: crate::entities::account::Account
//! [`Translation`]: crate::entities::translation::Translation

use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    routing::{delete, get, post, put},
};
use uuid::Uuid;

use crate::{
    entities::status::Status,
    error::{AppError, AppResult},
    models::{block, status, user::User},
    services::render_status::{ViewerMode, render_status},
    state::AppState,
};

use self::{
    bookmarks::{bookmark_status, list_bookmarks, unbookmark_status},
    context::get_status_context,
    create::create_status,
    delete_edit::{delete_status, edit_status},
    favourites::{favourite_status, list_favourited_by, list_favourites, unfavourite_status},
    history::{get_status_history, get_status_source},
    pins_mutes::{mute_conversation, pin_status, unmute_conversation, unpin_status},
    reblogs::{list_quotes, list_reblogged_by, reblog_status, unreblog_status},
    show::{get_status_card, list_statuses, show_status},
    translate::translate_status,
};

pub(crate) use self::{
    delete_edit::after_status_edited, favourites::after_favourite_changed,
    reblogs::after_reblog_changed,
};

mod bookmarks;
mod context;
mod create;
pub mod delete_edit;
pub mod favourites;
mod history;
pub mod pins_mutes;
pub mod reblogs;
mod show;
#[cfg(test)]
mod test_support;
mod translate;

/// How many ids one batch lookup acts on: Pleroma's `index` `limit`, with
/// every id past it ignored rather than refused.
pub const MAX_STATUS_IDS: usize = 100;

/// How many reply links either walk of `GET …/context` follows.
///
/// The thread depth this endpoint walks, up or down: a chain longer than
/// this reads as if the oldest ancestors (or the deepest replies) did not
/// exist.
pub const MAX_CONTEXT_DEPTH: i32 = 100;

/// How many rows the descendant walk of `GET …/context` collects.
///
/// The cap applies to the raw walk, before any filtering, so a thread
/// wider than this answers its oldest walked rows and drops the rest.
pub const MAX_CONTEXT_DESCENDANTS: i64 = 1000;

/// The 503 body of `POST …/translate` on an instance with no translation
/// provider configured (Pleroma's "Translation service not configured", in
/// the ticket's wording).
const NOT_ENABLED: &str = "Translation is not enabled";
/// The 503 body of `POST …/translate` when a configured provider did not
/// answer, or answered an error: the reason goes to the log, never here.
const UNAVAILABLE: &str = "Translation service unavailable";
/// The language `POST …/translate` translates into when the body names none
/// and the caller's account has no default of its own either.
const DEFAULT_TARGET_LANGUAGE: &str = "en";

/// The body of the delete, edit and source routes' ownership refusals:
/// Pleroma's `StatusController.delete` answers `{:error, :not_found}` for
/// an activity it cannot find, and `CommonAPI.delete`'s ownership arm — the
/// one that [`AppError::Forbidden`] status carries here — renders the same
/// `"Record not found"` wording, so a probe cannot tell "not yours" from
/// "not there". The edit and the source read keep that twin deliberately:
/// Pleroma's `update` refuses a status that is not the caller's own (and a
/// remote one, whose author never is) through the same body, and the
/// ticket pins it for `PUT` and `GET …/source` alike.
const RECORD_NOT_FOUND: &str = "Record not found";

/// The 422 body of `PUT /api/v1/statuses/{id}` for a reblog row: Pleroma's
/// `update` asks `activity.data["type"] == "Create"` before anything else
/// (the ticket's 422 with its own words), and decision 6's boost carries no
/// text of its own — there is nothing to revise, only a post to un-boost.
const CANNOT_EDIT_REBLOG: &str = "Cannot edit a reblog";

/// The 422 body of `POST …/pin` for a status of somebody else's: Pleroma's
/// `CommonAPI.pin` ownership arm (`{:error, :ownership_error}`), which its
/// `StatusController.pin` renders in exactly these words.
const NOT_YOURS: &str = "Someone else's status cannot be pinned";
/// The 422 body of `POST …/unpin` for the same case, Pleroma's `unpin`
/// ownership arm — the withdrawal is refused for the same reason the pin
/// is, and a pin of somebody else's post cannot exist to begin with.
const NOT_YOURS_UNPIN: &str = "Someone else's status cannot be unpinned";
/// The 422 body of `POST …/pin` for a `private`, `direct` or `local`
/// status: Pleroma's `{:error, :non_public_error}` arm, whose wording the
/// ticket pins (its `Visibility.public?/1` admits `public` and `unlisted`
/// and nothing else).
const NOT_PUBLIC: &str = "Non-public status cannot be pinned";
/// The 422 body of `POST …/pin` for a reblog row: Pleroma's
/// `object_type_is_allowed_for_pin` admits an `Announce`'s object as
/// little as a flag's, and the ticket gives the refusal its own words.
/// A reply is allowed, as it is there.
const NO_REBLOG: &str = "Reblogs cannot be pinned";
/// The 422 body of `POST …/pin` past
/// [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses):
/// Pleroma's `:pinned_statuses_limit_reached` arm, whose message its
/// `User.add_pinned_object_id` carries verbatim.
const PIN_LIMIT: &str = "You have already pinned the maximum number of statuses";

/// The 422 body of `POST …/reblog` for a `visibility` parameter outside
/// the three the boost takes: only `public`, `unlisted` and `private` are
/// spellings a reblog can wear, and anything else — `direct` and `local`
/// included — is refused in these words rather than quietly defaulted.
const INVALID_VISIBILITY: &str = "Invalid visibility";
/// The 422 body of `POST …/reblog` for a `private` or `direct` status: the
/// ticket's wording of Pleroma's refusal to `Announce` anything but a
/// publicly-addressed object — a boost is for the followers and the world,
/// never for a followers-only or addressed post, the caller's own included.
///
/// Shared with the C2S outbox's `Announce` arm in
/// [`crate::api::activitypub`], which refuses the same targets in the same
/// words.
pub(crate) const CANNOT_REBLOG: &str = "Could not reblog private status";

/// Page size cap of `favourited_by`: the ticket pins 80, above
/// [`crate::pagination::MAX_LIMIT`], with Pleroma's global 20 as the
/// default the plain [`PageParams::from_query`] keeps.
const FAVOURITED_BY_MAX_LIMIT: i64 = 80;

/// The `/api/v1` statuses router, merged into [`crate::api::v1`].
///
/// `state` reaches the seven status writes that mount the
/// `statuses_actions` throttle bucket the way the reports route does (see
/// [`crate::rate_limit`]): the create one, the two favourite ones, the two
/// reblog ones and `DELETE /api/v1/statuses/{id}` — Pleroma's
/// `@rate_limited_status_actions` keeps the new post, those state changes
/// and the report filing in one window. The other writes ride no bucket —
/// the two thread-mute ones like the bookmark and pin ones do, because a
/// thread mute is a private per-account view choice no client can hammer on
/// anyone else's data with; the tickets pin the throttle for the create
/// write, the like writes, the boost writes and the delete only, and a
/// bookmark is a private per-account fact — which a pin is too, since
/// `pin_refusal` admits only the caller's own statuses. The edit write is
/// the same kind of own-post-only change (`edit_refusal` admits nothing but
/// the caller's local posts, and the history and source reads behind it are
/// plain reads), and its ticket pins no bucket for it either.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/favourites", get(list_favourites))
        .route("/bookmarks", get(list_bookmarks))
        .route("/statuses/{id}", get(show_status))
        .route("/statuses/{id}", put(edit_status))
        .route("/statuses/{id}/card", get(get_status_card))
        .route("/statuses/{id}/context", get(get_status_context))
        .route("/statuses/{id}/history", get(get_status_history))
        .route("/statuses/{id}/source", get(get_status_source))
        .route("/statuses/{id}/quotes", get(list_quotes))
        .route(
            "/statuses/{id}",
            delete(delete_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses", get(list_statuses))
        .route(
            "/statuses",
            post(create_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/statuses/{id}/favourite",
            post(favourite_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/statuses/{id}/unfavourite",
            post(unfavourite_status)
                .layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses/{id}/favourited_by", get(list_favourited_by))
        .route(
            "/statuses/{id}/reblog",
            post(reblog_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/statuses/{id}/unreblog",
            post(unreblog_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses/{id}/reblogged_by", get(list_reblogged_by))
        .route("/statuses/{id}/bookmark", post(bookmark_status))
        .route("/statuses/{id}/unbookmark", post(unbookmark_status))
        .route("/statuses/{id}/pin", post(pin_status))
        .route("/statuses/{id}/unpin", post(unpin_status))
        .route("/statuses/{id}/mute", post(mute_conversation))
        .route("/statuses/{id}/unmute", post(unmute_conversation))
        .route("/statuses/{id}/translate", post(translate_status))
}

/// The `/api/v1/pleroma/statuses` side of the statuses router, nested in
/// [`crate::api::v1`] alongside [`crate::api::emoji_reactions`]'s router.
///
/// Pleroma serves the quoting listing under both prefixes
/// (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`'s
/// `quotes` action, routed in the mastodon and the pleroma scopes alike),
/// so this one route is the counterpart of the `/api/v1` path's listing.
/// Like the Mastodon mount it is a read and rides no throttle bucket.
pub fn pleroma_router() -> Router<AppState> {
    Router::new().route("/{id}/quotes", get(list_quotes))
}

/// The one status a write answers: the row `viewer` just changed —
/// a reblog row still answers *itself* — re-read first so the render
/// carries the counter the write stepped, then rendered for them.
///
/// # Errors
///
/// [`AppError::NotFound`] when the row is gone — deleted between the
/// visibility gate and this re-read — and for the one row the renderer
/// answers `None` for (a reblog whose original went away in the same
/// window; [`crate::api::emoji_reactions`] answers it the same way); any
/// database failure arrives through `?` as [`AppError::Database`].
async fn current_entity(state: &AppState, viewer: &User, id: Uuid) -> AppResult<Json<Status>> {
    let row = status::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let entity = render_status(state, ViewerMode::user(viewer), &row)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(entity))
}

/// `entries` without the accounts `viewer` has blocked, in the same order;
/// `account_id` names the account behind one entry. An anonymous viewer has
/// no blocks, so the list comes back as it was (Pleroma's `favourited_by`
/// and `reblogged_by` drop the viewer's blocked accounts the same way).
///
/// The listings call it *after* the `Link` header is computed from the raw
/// page, so a page emptied by the filter still names its cursors and the
/// walk does not stall.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn without_blocked<T>(
    state: &AppState,
    viewer: Option<&User>,
    entries: Vec<T>,
    account_id: impl Fn(&T) -> Uuid,
) -> AppResult<Vec<T>> {
    let Some(viewer) = viewer else {
        return Ok(entries);
    };
    let blocked: HashSet<Uuid> = block::blocked_ids(&state.db, viewer.id)
        .await?
        .into_iter()
        .collect();
    if blocked.is_empty() {
        return Ok(entries);
    }
    Ok(entries
        .into_iter()
        .filter(|entry| !blocked.contains(&account_id(entry)))
        .collect())
}

/// The status a like or a boost lands on: `row` itself for a plain post,
/// and its **original** for a reblog row — Mastodon's behaviour for liking
/// a boost, mirrored here for the like and for the reblog write
/// (nobody boosts a boost), and in the renderer's flag lookups, where the
/// same rule keys `favourited` and `reblogged`. The endpoints still answer
/// the row the client named.
///
/// `pub(crate)` because the C2S outbox in [`crate::api::activitypub`] folds
/// its posted `Like`s and `Announce`s with the very same rule.
pub(crate) const fn action_target_id(row: &status::Status) -> Uuid {
    match row.reblog_of_id {
        Some(original) => original,
        None => row.id,
    }
}

/// The `visible` rows in the order `requested` names them, each id answered
/// at most once: an id with no row here (unknown, deleted, invisible)
/// contributes nothing, and one named twice contributes one entry, which is
/// what Pleroma's `WHERE id IN (…)` read does for a duplicated id.
fn in_requested_order(requested: &[Uuid], visible: Vec<status::Status>) -> Vec<status::Status> {
    let mut by_id: HashMap<Uuid, status::Status> =
        visible.into_iter().map(|row| (row.id, row)).collect();
    requested.iter().filter_map(|id| by_id.remove(id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::statuses::test_support::{ids, row},
        pagination::link_for_page,
    };

    #[test]
    fn the_answer_follows_the_request_not_the_row_order() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let visible = vec![row(b), row(c), row(a)];
        assert_eq!(
            ids(&in_requested_order(&[a, b, c], visible)),
            vec![a, b, c],
            "rows the batch read answered in id order come back in the \
             order they were asked for"
        );
    }

    #[test]
    fn unknown_ids_drop_out_and_duplicates_answer_once() {
        let (a, b, gone) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let visible = vec![row(b), row(a)];
        assert_eq!(
            ids(&in_requested_order(&[gone, b, a, a], visible)),
            vec![b, a],
            "an id with no visible row contributes nothing, and one asked \
             twice contributes one entry"
        );
    }

    #[test]
    fn nothing_visible_is_the_empty_page() {
        let requested = [Uuid::now_v7()];
        assert!(
            in_requested_order(&requested, Vec::new()).is_empty(),
            "a request whose every id was skipped answers the empty page"
        );
    }

    #[test]
    fn the_link_pages_off_the_first_and_last_like_ids() {
        let (newest, middle, oldest) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let link = link_for_page(
            "https://host",
            "/api/v1/favourites",
            &[newest, middle, oldest],
            |id| *id,
        )
        .expect("a page with ids links");
        assert_eq!(
            link,
            format!(
                "<https://host/api/v1/favourites?max_id={oldest}>; rel=\"next\", \
                 <https://host/api/v1/favourites?min_id={newest}>; rel=\"prev\""
            ),
            "next walks to the oldest like, prev back to the newest, and \
             the middle id names nothing"
        );
    }

    #[test]
    fn an_empty_page_links_nowhere() {
        let none: [Uuid; 0] = [];
        assert_eq!(
            link_for_page("https://host", "/api/v1/favourites", &none, |id| *id),
            None,
            "no rows, no cursors, no Link header"
        );
    }

    #[test]
    fn a_reblog_like_is_keyed_to_its_target() {
        // The mapping all four writes hand their model: the reblog row's
        // id is what the client named, the original's is where the like —
        // or the boost — lives.
        let (reblog, original) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            action_target_id(&row(reblog)),
            reblog,
            "a plain post likes itself"
        );
        let mut boost = row(reblog);
        boost.reblog_of_id = Some(original);
        assert_eq!(
            action_target_id(&boost),
            original,
            "a boost is liked through its original"
        );
    }
}
