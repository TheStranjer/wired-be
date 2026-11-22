//! The `/api/v1/timelines/*` status timelines, each one the
//! [`TimelineQuery`] of [`crate::services::timeline`] rendered as a page.
//!
//! The endpoints share one helper, [`timeline_response`], which fetches,
//! renders and links the page; the routes differ only in what they ask the
//! builder for.
//!
//! `GET /api/v1/timelines/tag/{tag}` (WBE-102) answers the `public` statuses
//! carrying a tag: the path segment is URL-decoded, lowercased and stripped
//! of its leading `#` (the spelling [`crate::models::hashtag`] stores), and
//! what is left must name a match — an empty name is the 404. The request
//! can widen that match with `any[]`, narrow it to rows also carrying every
//! `all[]` tag and drop rows carrying any `none[]` tag (the path tag always
//! rides in the `any` list, Pleroma's `[params[:tag], params[:any]]`, so the
//! timeline is never untagged), and narrow it by origin (`local`, `remote`),
//! attachments (`only_media`), visibility (`exclude_visibilities[]`) and the
//! viewer's own exclusions (`with_muted`). Auth is optional: an anonymous
//! request reads the same public page, a presented user token must grant
//! `read:statuses` (Pleroma's `OAuthScopesPlug` on the `hashtag` action).
//!
//! `GET /api/v1/timelines/public` (WBE-101) answers the
//! [`TimelineSource::Public`] page — `public` statuses, and `local` ones too
//! for an authenticated viewer this instance owns — narrowed by `local`,
//! `remote`, `only_media`, `exclude_visibilities[]`, `with_muted` and
//! `reply_visibility` (Pleroma's `following`/`self` reply filter), and by
//! `instance`, Pleroma's author-domain extension. Auth is optional under the
//! same `read:statuses` rule; on top of it the instance can close the two
//! anonymous reads through `RESTRICT_UNAUTHENTICATED` (see
//! [`Config::restricts`]): while `timelines_local` is listed, an
//! unauthenticated `local=true` request is refused, and while
//! `timelines_federated` is listed, every other unauthenticated request is.
//! The refusal is the 403
//! `{"error":"Authentication is required to access this resource."}` —
//! Pleroma answers `401 "authorization required for timeline view"`
//! (`fail_on_bad_auth`), but the ticket body is authoritative.
//!
//! `GET /api/v1/timelines/direct` (WBE-103) answers the viewer's own
//! conversations: the `direct` statuses they authored or are @-mentioned
//! in, newest first, one [`PageParams`] page at a time (`limit` defaults
//! to 20 and caps at 40). Auth is required (Pleroma's `OAuthScopesPlug`
//! with `scopes: ["read:statuses"]` behind the user requirement on the
//! `direct` action): no token is the extractor's 401, an app-only token
//! and a user token without the scope its 403. Statuses authored by
//! accounts the viewer blocked or still mutes stay hidden unless
//! `with_muted=true`, and the `Link` header works exactly as on the tag
//! route.
//!
//! `GET /api/v1/timelines/list/{list_id}` (WBE-105) answers one of the
//! caller's lists: statuses authored by its members — membership alone,
//! no follow required — reblog rows by members included, replies narrowed
//! by the list's stored `replies_policy` (see
//! [`TimelineSource::List`]). Auth is required ([`AuthUser`] with
//! `read:lists`, Pleroma's scopes plug on the `list` action), and a list
//! that is malformed, unknown or someone else's is one and the same 404
//! "Record not found". `only_media` and `with_muted` narrow the page like
//! they do everywhere else, and the `Link` header works exactly as on the
//! other routes.
//!
//! `GET /api/v1/timelines/home` (WBE-100) answers the viewer's following
//! timeline: the statuses they authored themselves — their reblogs
//! included — plus those of every account they hold an accepted follow of,
//! a follow's reblog rows only while that follow still shows reblogs (see
//! [`TimelineSource::Home`]); replies to accounts the viewer does not
//! follow ride along until `reply_visibility` says otherwise. Auth is
//! required (Pleroma's `OAuthScopesPlug` with `scopes: ["read:statuses"]`
//! behind the user requirement on the `home` action): no token is the
//! extractor's 401, an app-only token and a user token without the scope
//! its 403. `local`, `remote`, `only_media`, `with_muted` and
//! `exclude_visibilities[]` narrow the page exactly as on the tag route,
//! `reply_visibility` takes only `following` and `self` (any other
//! spelling is the 422) and `exclude_replies` is not a home parameter and
//! is ignored. The `Link` header works exactly as on the other routes.
//!
//! Reference: `lib/pleroma/web/mastodon_api/controllers/timeline_controller.ex`
//! (`hashtag/2`, `hashtag_fetching/3`, `public/2`, `direct/2`, `list/2`
//! and `home/2`, with its `restrict_unauthenticated?/1`) and
//! `lib/pleroma/web/plugs/ensure_public_or_authenticated_plug.ex`; the
//! ticket body is authoritative.

use axum::{Router, routing::get};

use crate::{
    error::{AppError, AppResult},
    services::timeline::ReplyVisibility,
    state::AppState,
};

#[cfg(doc)]
use crate::{
    api::helpers::timeline_response,
    auth::AuthUser,
    config::Config,
    pagination::PageParams,
    services::timeline::{TimelineQuery, TimelineSource},
};

mod direct_list;
mod home;
mod public;
mod tag;
#[cfg(test)]
mod test_support;

use direct_list::{direct_timeline, list_timeline};
use home::home_timeline;
use public::public_timeline;
use tag::tag_timeline;

/// The 403 body a `RESTRICT_UNAUTHENTICATED` gate answers an
/// unauthenticated timeline read with.
///
/// The ticket's exact wording; Pleroma's own refusal is the 401
/// "authorization required for timeline view" (`fail_on_bad_auth`).
const AUTHENTICATION_REQUIRED: &str = "Authentication is required to access this resource.";

/// The `RESTRICT_UNAUTHENTICATED` area of the `local=true` read of
/// `GET /api/v1/timelines/public` (Pleroma's `timelines: %{local: …}`).
const TIMELINES_LOCAL: &str = "timelines_local";

/// The [`TIMELINES_LOCAL`] twin for every other anonymous read of the
/// public timeline (Pleroma's `timelines: %{federated: …}`).
const TIMELINES_FEDERATED: &str = "timelines_federated";

/// The `/api/v1/timelines` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/timelines/public", get(public_timeline))
        .route("/timelines/home", get(home_timeline))
        .route("/timelines/tag/{tag}", get(tag_timeline))
        .route("/timelines/direct", get(direct_timeline))
        .route("/timelines/list/{list_id}", get(list_timeline))
}

/// 422 body for a `reply_visibility` outside `following` and `self`
/// (Mastodon's changeset wording, the shape [`crate::api::lists`] gives
/// its own rejected enum parameter).
const REPLY_VISIBILITY_INVALID: &str =
    "Validation failed: Reply visibility is not included in the list";

/// One `reply_visibility` value: `following` is
/// [`ReplyVisibility::Following`] and `self` [`ReplyVisibility::SelfOnly`]
/// (the two spellings Pleroma's `timeline_operation` enum lists, verbatim
/// — any other text, empties and capitals included, is the endpoint's
/// 422 [`REPLY_VISIBILITY_INVALID`]).
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn parse_reply_visibility(text: &str) -> AppResult<ReplyVisibility> {
    match text {
        "following" => Ok(ReplyVisibility::Following),
        "self" => Ok(ReplyVisibility::SelfOnly),
        _ => Err(AppError::Unprocessable(REPLY_VISIBILITY_INVALID.to_owned())),
    }
}

/// The [`ReplyVisibility`] a public-page `reply_visibility` parameter names.
///
/// Pleroma's two values `following` and `self`, trimmed and spelled exactly
/// (the way [`push_visibility`] takes only the verbatim visibility names);
/// anything else — `all`, `ignored`, junk — keeps every reply, which is the
/// parameter's absence. The public page ignores an unknown spelling where
/// [`parse_reply_visibility`] (the home route's) rejects it with a 422.
fn parse_public_reply_visibility(text: &str) -> Option<ReplyVisibility> {
    match text.trim() {
        "following" => Some(ReplyVisibility::Following),
        "self" => Some(ReplyVisibility::SelfOnly),
        _ => None,
    }
}

/// The `instance` domain a parameter names, trimmed.
///
/// An empty text reads as no filter at all: a blank domain would ask for
/// rows nobody lives under, and Pleroma's free-text parameter with no value
/// never reaches the query as one.
fn parse_instance(text: &str) -> Option<String> {
    let domain = text.trim();
    (!domain.is_empty()).then(|| domain.to_owned())
}
