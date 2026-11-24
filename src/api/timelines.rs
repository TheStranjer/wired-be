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
//! `read:statuses` (Pleroma's `OAuthScopesPlug` on the `hashtag` action). The
//! same `RESTRICT_UNAUTHENTICATED` gate as the public timeline's (see below,
//! `ensure_open`) refuses an anonymous tag read, keyed on `local`.
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
    api::helpers::{QueryPairs, is_true, push_visibility},
    config::Config,
    error::{AppError, AppResult},
    models::user::Flag,
    services::timeline::{ReplyVisibility, TimelineFilters},
    state::AppState,
};

#[cfg(doc)]
use crate::{
    api::helpers::timeline_response,
    auth::AuthUser,
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

/// The `RESTRICT_UNAUTHENTICATED` area an anonymous read of the public
/// timeline falls under: [`TIMELINES_LOCAL`] for the local page, and
/// [`TIMELINES_FEDERATED`] for every other one.
///
/// The REST gate and the streaming gate both choose their area here, so the
/// two cannot drift apart.
pub(crate) const fn timeline_area(local: bool) -> &'static str {
    if local {
        TIMELINES_LOCAL
    } else {
        TIMELINES_FEDERATED
    }
}

/// Pleroma's `restrict_unauthenticated?` gate, in `timeline_controller.ex`,
/// shared by the public and the hashtag timelines.
///
/// With no user behind the request, the `local=true` page belongs to the
/// [`TIMELINES_LOCAL`] area and every other read to [`TIMELINES_FEDERATED`]
/// (chosen by [`timeline_area`]), and a listed area refuses the request
/// [`AUTHENTICATION_REQUIRED`]. An authenticated request (`authenticated` — a
/// live *user* token was presented; an app-only one is unauthenticated here,
/// as in Pleroma's `conn.assigns[:user]`) and every unlisted area pass.
/// `local` is the request's `local` parameter, a [`Flag`] to keep the
/// signature free of a second bare `bool`.
///
/// # Errors
///
/// [`AppError::Forbidden`] with [`AUTHENTICATION_REQUIRED`] when the
/// anonymous read of this page's area is restricted.
fn ensure_open(config: &Config, authenticated: bool, local: Flag) -> AppResult<()> {
    if authenticated {
        return Ok(());
    }
    if config.restricts(timeline_area(local.get())) {
        return Err(AppError::Forbidden(AUTHENTICATION_REQUIRED.to_owned()));
    }
    Ok(())
}

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

/// The five narrowings every timeline route shares, read out of `pairs`.
///
/// `local`, `remote`, `only_media` and `with_muted` are [`Flag`]s read with
/// [`is_true`] (the last occurrence of a key wins, an empty value is off),
/// and `exclude_visibilities` fills from both the bracketed and the bare
/// spelling in occurrence order through [`push_visibility`], so a name that
/// is not one of the five stored visibilities narrows nothing. Every other
/// [`TimelineFilters`] field stays at its [`Default`]; a route sets its own
/// extras on top with struct-update syntax, and one that honours fewer than
/// these five clears the rest.
fn common_filters(pairs: &QueryPairs) -> TimelineFilters {
    let flag = |key: &str| Flag::new(pairs.value(key).is_some_and(|text| is_true(&text)));
    let mut exclude_visibilities = Vec::new();
    for name in pairs.values("exclude_visibilities") {
        push_visibility(&mut exclude_visibilities, &name);
    }
    TimelineFilters {
        local_only: flag("local"),
        remote_only: flag("remote"),
        only_media: flag("only_media"),
        with_muted: flag("with_muted"),
        exclude_visibilities,
        ..TimelineFilters::default()
    }
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse as _};

    use super::*;
    use crate::models::status::Visibility;

    /// A [`Config`] whose `RESTRICT_UNAUTHENTICATED` lists `areas`.
    fn restricted(areas: &[&str]) -> Config {
        let mut config = Config::for_tests();
        config.restrict_unauthenticated = areas.iter().map(|area| (*area).to_owned()).collect();
        config
    }

    #[test]
    fn the_restriction_gate_leaves_every_open_read_through() {
        let open = Config::for_tests();
        ensure_open(&open, false, Flag::new(true))
            .expect("an open instance answers an anonymous `local=true`");
        ensure_open(&open, false, Flag::new(false))
            .expect("an open instance answers an anonymous federated read");
        ensure_open(&open, true, Flag::new(false))
            .expect("an authenticated read was never the gate's business");
    }

    #[test]
    fn timelines_local_restricted_refuses_only_the_anonymous_local_read() {
        let config = restricted(&[TIMELINES_LOCAL]);
        let refusal = ensure_open(&config, false, Flag::new(true))
            .expect_err("the anonymous `local=true` read is refused");
        assert_eq!(
            refusal.to_string(),
            AUTHENTICATION_REQUIRED,
            "the refusal states the ticket's exact wording"
        );
        assert_eq!(
            refusal.into_response().status(),
            StatusCode::FORBIDDEN,
            "and answers it as a 403"
        );
        ensure_open(&config, false, Flag::new(false))
            .expect("a non-local read is the federated page, and that one is open");
        ensure_open(&config, true, Flag::new(true))
            .expect("an authenticated `local=true` read passes");
    }

    #[test]
    fn timelines_federated_restricted_refuses_every_other_anonymous_read() {
        let config = restricted(&[TIMELINES_FEDERATED]);
        let refusal = ensure_open(&config, false, Flag::new(false))
            .expect_err("the anonymous federated read is refused");
        assert_eq!(
            refusal.to_string(),
            AUTHENTICATION_REQUIRED,
            "the refusal is the same wording"
        );
        ensure_open(&config, false, Flag::new(true))
            .expect("the `local=true` page belongs to the other area and stays open");
        ensure_open(&config, true, Flag::new(false))
            .expect("an authenticated federated read passes");
    }

    #[test]
    fn both_areas_restricted_refuse_the_whole_anonymous_timeline() {
        let config = restricted(&[TIMELINES_LOCAL, TIMELINES_FEDERATED]);
        for local in [Flag::new(false), Flag::new(true)] {
            let refusal = ensure_open(&config, false, local)
                .expect_err("a listed area refuses every anonymous read");
            assert_eq!(
                refusal.to_string(),
                AUTHENTICATION_REQUIRED,
                "both areas listed, every anonymous read is the same refusal"
            );
            ensure_open(&config, true, local)
                .expect("and an authenticated read passes whatever is listed");
        }
    }

    /// [`common_filters`] over a raw query string.
    fn common(query: &str) -> TimelineFilters {
        common_filters(&QueryPairs::parse(Some(query)))
    }

    #[test]
    fn no_query_is_every_filter_off() {
        for query in [
            "",
            "foo=bar&limit=5&reply_visibility=self&instance=x.example",
        ] {
            assert_eq!(
                common(query),
                TimelineFilters::default(),
                "`{query}` narrows nothing"
            );
        }
        assert_eq!(
            common_filters(&QueryPairs::parse(None)),
            TimelineFilters::default(),
            "an absent query is no pairs"
        );
    }

    #[test]
    fn the_four_flags_read_truthy_text_and_the_last_occurrence_wins() {
        let on = common("local=true&remote=1&only_media=TRUE&with_muted=yes");
        assert!(on.local_only.get(), "`local=true`");
        assert!(on.remote_only.get(), "`remote=1`");
        assert!(on.only_media.get(), "`only_media` is case-insensitive");
        assert!(on.with_muted.get(), "`yes` is a true spelling");
        let off = common("local=false&remote=&only_media=maybe&with_muted=true&with_muted=false");
        assert!(
            !off.local_only.get()
                && !off.remote_only.get()
                && !off.only_media.get()
                && !off.with_muted.get(),
            "`false`, empty and junk are off and the last occurrence wins"
        );
        assert!(
            !common("local=true&local=").local_only.get(),
            "a later empty value overrides an earlier true one"
        );
    }

    #[test]
    fn exclude_visibilities_fills_from_both_spellings_in_order() {
        let filters = common(
            "exclude_visibilities[]=public&exclude_visibilities=private\
             &exclude_visibilities[]=direct",
        );
        assert_eq!(
            filters.exclude_visibilities,
            vec![Visibility::Public, Visibility::Private, Visibility::Direct],
            "the bracketed and the bare spelling fill one list, in order"
        );
        assert_eq!(
            common("exclude_visibilities=unlisted").exclude_visibilities,
            vec![Visibility::Unlisted],
            "the bare spelling alone works"
        );
        assert_eq!(
            common("exclude_visibilities[]=unlisted").exclude_visibilities,
            vec![Visibility::Unlisted],
            "the bracketed spelling alone works"
        );
    }

    #[test]
    fn exclude_visibilities_ignores_names_that_are_not_stored_spellings() {
        let filters = common(
            "exclude_visibilities[]=Public&exclude_visibilities[]=&exclude_visibilities=nope\
             &exclude_visibilities[]=direct",
        );
        assert_eq!(
            filters.exclude_visibilities,
            vec![Visibility::Direct],
            "a capitalization, an empty value and junk narrow nothing"
        );
    }

    #[test]
    fn the_route_extras_are_left_at_their_defaults() {
        let filters = common("local=true&reply_visibility=self&instance=x.example");
        assert_eq!(filters.reply_visibility, None, "the route's own extra");
        assert_eq!(filters.instance, None, "the route's own extra");
        assert!(!filters.exclude_replies.get(), "never a shared key");
    }
}
