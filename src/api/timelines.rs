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
//! (`hashtag/2`, `hashtag_fetching/3`, `direct/2`, `list/2` and `home/2`);
//! the ticket body is authoritative. The later public route joins this
//! module and reuses [`timeline_response`].

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::get,
};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    api::helpers::{is_true, normalize_tag, push_tag, push_visibility, timeline_response},
    auth::{AuthUser, MaybeAuthUser},
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    models::{
        list,
        status::Visibility,
        user::{Flag, User},
    },
    oauth::scopes::{READ_LISTS, READ_STATUSES},
    pagination::PageParams,
    services::timeline::{ReplyVisibility, TimelineFilters, TimelineQuery, TimelineSource},
    state::AppState,
    util::id::parse_id,
};

/// The `/api/v1/timelines` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/timelines/home", get(home_timeline))
        .route("/timelines/tag/{tag}", get(tag_timeline))
        .route("/timelines/direct", get(direct_timeline))
        .route("/timelines/list/{list_id}", get(list_timeline))
}

/// `GET /api/v1/timelines/tag/{tag}` — the `public` statuses carrying `tag`,
/// newest first, one [`PageParams`] keyset page at a time. Auth is optional
/// (Pleroma's `read:statuses` scopes plug with
/// `fallback: :proceed_unauthenticated`): a presented user token must grant
/// the scope, an app-only token and no token at all both read anonymously,
/// and an invalid token is the extractor's 401.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// `Uri` the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the presented user token lacks `read:statuses`; [`AppError::NotFound`]
/// for a path tag that normalizes to nothing; [`AppError::BadRequest`] from
/// [`PageParams`] for a malformed cursor. Any database failure arrives
/// through `?` as [`AppError::Database`], and [`set_link_header`](crate::pagination::set_link_header) reports a
/// `Link` value that is not header-safe as [`AppError::Internal`].
async fn tag_timeline(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(raw_tag): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let tag = normalize_tag(&raw_tag).ok_or(AppError::NotFound)?;
    let query = TagParams::parse(uri.query()).into_timeline(tag, auth.0, page);
    timeline_response(&state, &query, &uri).await
}

/// The hashtag timeline's request parameters beyond the path tag and the
/// [`PageParams`] page: the extra tag arrays and the narrowings, every one
/// of them a no-op in its [`Default`] spelling. The booleans are [`Flag`]s
/// because that is what this repository models every boolean with (see
/// [`crate::models::user`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TagParams {
    /// `any[]`: extra tags that join the path tag in the union every row
    /// must carry at least one of.
    any: Vec<String>,
    /// `all[]`: every row must carry each of these tags on top of the match
    /// the first two fields decide.
    all: Vec<String>,
    /// `none[]`: a row carrying any of these tags is dropped.
    none: Vec<String>,
    /// `local`: keep only statuses this instance authored.
    local: Flag,
    /// `remote`: keep only statuses mirrored from other instances.
    remote: Flag,
    /// `only_media`: keep only statuses carrying an attachment.
    only_media: Flag,
    /// `with_muted`: skip the viewer's block/mute exclusions (Pleroma's
    /// `with_muted`, default `false`).
    with_muted: Flag,
    /// `exclude_visibilities[]`: drop rows whose visibility is one of
    /// these; names that are not one of the five stored spellings are
    /// ignored.
    exclude_visibilities: Vec<Visibility>,
}

impl TagParams {
    /// Reads the parameters out of a raw query string (what
    /// `Uri::query` returns, percent-decoding and all). Every array key
    /// is read in both the bracketed spelling Mastodon clients send and the
    /// bare one Pleroma's `Plug` accepts (`any` and `any[]` fill the same
    /// list, in occurrence order); a value that normalizes to nothing is
    /// dropped, and an unrelated key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let mut found = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "any" | "any[]" => push_tag(&mut found.any, &value),
                "all" | "all[]" => push_tag(&mut found.all, &value),
                "none" | "none[]" => push_tag(&mut found.none, &value),
                "local" => found.local = Flag::new(is_true(&value)),
                "remote" => found.remote = Flag::new(is_true(&value)),
                "only_media" => found.only_media = Flag::new(is_true(&value)),
                "with_muted" => found.with_muted = Flag::new(is_true(&value)),
                "exclude_visibilities" | "exclude_visibilities[]" => {
                    push_visibility(&mut found.exclude_visibilities, &value);
                }
                _ => {}
            }
        }
        found
    }

    /// Assembles the [`TimelineQuery`] the request asks for: the path `tag`
    /// leads the `any` list, so it is part of the match together with every
    /// `any[]` name (Pleroma's `[params[:tag], params[:any]]`), and
    /// `viewer` is the authenticated account or `None` for an anonymous
    /// read.
    fn into_timeline(self, tag: String, viewer: Option<User>, page: PageParams) -> TimelineQuery {
        let mut any = Vec::with_capacity(self.any.len() + 1);
        any.push(tag);
        any.extend(self.any);
        TimelineQuery {
            viewer,
            source: TimelineSource::Hashtag {
                any,
                all: self.all,
                none: self.none,
            },
            filters: TimelineFilters {
                local_only: self.local,
                remote_only: self.remote,
                only_media: self.only_media,
                with_muted: self.with_muted,
                exclude_visibilities: self.exclude_visibilities,
                ..TimelineFilters::default()
            },
            page,
        }
    }
}

/// `GET /api/v1/timelines/direct` — the `direct` statuses the viewer
/// authored or is @-mentioned in, newest first, one [`PageParams`] keyset
/// page at a time. Auth is required ([`AuthUser`], Pleroma's user
/// requirement on the `direct` action): no token is its 401 "Invalid
/// credentials", an app-only token the 403 "This API requires an
/// authenticated user", and a user token must grant `read:statuses`
/// ([`AuthUser::require`]).
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// `Uri` the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`AuthUser::require`] when the token lacks
/// `read:statuses`; [`AppError::BadRequest`] from [`PageParams`] for a
/// malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn direct_timeline(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require(READ_STATUSES)?;
    let query = DirectParams::parse(uri.query()).into_timeline(auth.0, page);
    timeline_response(&state, &query, &uri).await
}

/// The direct timeline's request parameters beyond the [`PageParams`]
/// page: just `with_muted`, the one toggle Pleroma's `direct/2` reads
/// besides the cursors. It is a [`Flag`] because that is what this
/// repository models every boolean with (see [`crate::models::user`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DirectParams {
    /// `with_muted`: skip the viewer's block/mute exclusions (Pleroma's
    /// `with_muted`, default `false`).
    with_muted: Flag,
}

impl DirectParams {
    /// Reads the parameters out of a raw query string (what `Uri::query`
    /// returns, percent-decoding and all); the cursors and `limit` are
    /// [`PageParams`]'s to read and every other key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let mut found = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            if key == "with_muted" {
                found.with_muted = Flag::new(is_true(&value));
            }
        }
        found
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::Direct`] as `viewer`, who is always present
    /// because the route requires authentication, with only the
    /// `with_muted` filter set from the query.
    fn into_timeline(self, viewer: User, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer: Some(viewer),
            source: TimelineSource::Direct,
            filters: TimelineFilters {
                with_muted: self.with_muted,
                ..TimelineFilters::default()
            },
            page,
        }
    }
}

/// `GET /api/v1/timelines/list/{list_id}` — the statuses authored by one
/// of the caller's lists' members (membership alone, no follow required),
/// replies narrowed by the list's stored `replies_policy`, newest first,
/// one [`PageParams`] keyset page at a time (`limit` defaults to 20 and
/// caps at 40). Auth is required ([`AuthUser`], Pleroma's user
/// requirement on the `list` action): no token is its 401 "Invalid
/// credentials", an app-only token the 403 "This API requires an
/// authenticated user", and a user token must grant `read:lists`
/// ([`AuthUser::require`]) — Pleroma's scopes plug for this action,
/// unlike the other timelines, asks the list scope, not `read:statuses`.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// `Uri` the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`AuthUser::require`] when the token lacks
/// `read:lists`; [`AppError::NotFound`] when `list_id` is malformed, names
/// no list or names another account's (see [`owned_list_id`], Pleroma
/// answering a foreign `fetch_activities` list with its generic 404);
/// [`AppError::BadRequest`] from [`PageParams`] for a malformed cursor.
/// Any database failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
async fn list_timeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(raw_id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require(READ_LISTS)?;
    let list_id = owned_list_id(&state, auth.0.id, &raw_id).await?;
    let query = ListParams::parse(uri.query()).into_timeline(list_id, auth.0, page);
    timeline_response(&state, &query, &uri).await
}

/// Resolves the `{list_id}` path segment to the id of a list `viewer_id`
/// owns: [`parse_id`]'s malformed spelling, an unknown id and a foreign
/// list are one and the same [`AppError::NotFound`] — Pleroma's
/// `FallbackController` answer, and the same rule every list route in
/// [`crate::api::lists`] lives by.
async fn owned_list_id(state: &AppState, viewer_id: Uuid, raw: &str) -> AppResult<Uuid> {
    let Some(list_id) = parse_id(raw) else {
        return Err(AppError::NotFound);
    };
    if list::get_owned(&state.db, viewer_id, list_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    Ok(list_id)
}

/// The list timeline's request parameters beyond the [`PageParams`] page
/// and the path id: the two toggles the ticket names. The booleans are
/// [`Flag`]s because that is what this repository models every boolean
/// with (see [`crate::models::user`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ListParams {
    /// `only_media`: keep only statuses carrying an attachment.
    only_media: Flag,
    /// `with_muted`: skip the viewer's block/mute exclusions (Pleroma's
    /// `with_muted`, default `false`).
    with_muted: Flag,
}

impl ListParams {
    /// Reads the parameters out of a raw query string (what `Uri::query`
    /// returns, percent-decoding and all); the cursors and `limit` are
    /// [`PageParams`]'s to read and every other key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let mut found = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "only_media" => found.only_media = Flag::new(is_true(&value)),
                "with_muted" => found.with_muted = Flag::new(is_true(&value)),
                _ => {}
            }
        }
        found
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::List`] of `list_id` as `viewer` — who is always
    /// present because the route requires authentication, and whose list
    /// `list_id` names because [`owned_list_id`] proved it — with the two
    /// toggles set from the query.
    fn into_timeline(self, list_id: Uuid, viewer: User, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer: Some(viewer),
            source: TimelineSource::List { list_id },
            filters: TimelineFilters {
                only_media: self.only_media,
                with_muted: self.with_muted,
                ..TimelineFilters::default()
            },
            page,
        }
    }
}

/// 422 body for a `reply_visibility` outside `following` and `self`
/// (Mastodon's changeset wording, the shape [`crate::api::lists`] gives
/// its own rejected enum parameter).
const REPLY_VISIBILITY_INVALID: &str =
    "Validation failed: Reply visibility is not included in the list";

/// `GET /api/v1/timelines/home` — the viewer's own posts and reblogs plus
/// those of their accepted follows, newest first, one [`PageParams`]
/// keyset page at a time (`limit` defaults to 20 and caps at 40). Auth is
/// required ([`AuthUser`], Pleroma's user requirement on the `home`
/// action): no token is its 401 "Invalid credentials", an app-only token
/// the 403 "This API requires an authenticated user", and a user token
/// must grant `read:statuses` ([`AuthUser::require`]).
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// `Uri` the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`AuthUser::require`] when the token lacks
/// `read:statuses`; [`AppError::Unprocessable`] from [`HomeParams::parse`]
/// for a `reply_visibility` outside `following` and `self`;
/// [`AppError::BadRequest`] from [`PageParams`] for a malformed cursor.
/// Any database failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
async fn home_timeline(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require(READ_STATUSES)?;
    let query = HomeParams::parse(uri.query())?.into_timeline(auth.0, page);
    timeline_response(&state, &query, &uri).await
}

/// The home timeline's request parameters beyond the [`PageParams`] page:
/// the four toggles, the visibility exclusions and the reply narrowing,
/// every one of them a no-op in its [`Default`] spelling. The booleans are
/// [`Flag`]s because that is what this repository models every boolean
/// with (see [`crate::models::user`]). `exclude_replies` is not a
/// parameter of this timeline (Pleroma's `home_operation` does not list
/// it), so [`HomeParams::parse`] never reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HomeParams {
    /// `local`: keep only statuses whose authors this instance owns.
    local: Flag,
    /// `remote`: keep only statuses whose authors are remote mirrors.
    remote: Flag,
    /// `only_media`: keep only statuses carrying an attachment.
    only_media: Flag,
    /// `with_muted`: skip the viewer's block/mute exclusions (Pleroma's
    /// `with_muted`, default `false`).
    with_muted: Flag,
    /// `exclude_visibilities[]`: drop rows whose visibility is one of
    /// these; names that are not one of the five stored spellings are
    /// ignored.
    exclude_visibilities: Vec<Visibility>,
    /// `reply_visibility`: which replies survive; `None` keeps them all,
    /// anything outside `following` and `self` is the parse's 422.
    reply_visibility: Option<ReplyVisibility>,
}

impl HomeParams {
    /// Reads the parameters out of a raw query string (what `Uri::query`
    /// returns, percent-decoding and all), reading the shared toggles as
    /// [`TagParams::parse`] does — `exclude_visibilities` in both the
    /// bracketed and the bare spelling — and mapping `reply_visibility`
    /// through [`parse_reply_visibility`]. The last occurrence of a key
    /// wins; the cursors and `limit` are [`PageParams`]'s to read and
    /// every other key is never looked at.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] from [`parse_reply_visibility`].
    fn parse(query: Option<&str>) -> AppResult<Self> {
        let mut found = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "local" => found.local = Flag::new(is_true(&value)),
                "remote" => found.remote = Flag::new(is_true(&value)),
                "only_media" => found.only_media = Flag::new(is_true(&value)),
                "with_muted" => found.with_muted = Flag::new(is_true(&value)),
                "exclude_visibilities" | "exclude_visibilities[]" => {
                    push_visibility(&mut found.exclude_visibilities, &value);
                }
                "reply_visibility" => {
                    found.reply_visibility = Some(parse_reply_visibility(&value)?);
                }
                _ => {}
            }
        }
        Ok(found)
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::Home`] as `viewer` — who is always present
    /// because the route requires authentication — with the toggles, the
    /// visibility exclusions and the reply narrowing set from the query.
    fn into_timeline(self, viewer: User, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer: Some(viewer),
            source: TimelineSource::Home,
            filters: TimelineFilters {
                local_only: self.local,
                remote_only: self.remote,
                only_media: self.only_media,
                with_muted: self.with_muted,
                exclude_visibilities: self.exclude_visibilities,
                reply_visibility: self.reply_visibility,
                ..TimelineFilters::default()
            },
            page,
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The parsed query as owned pairs, so the assertions read like the
    /// query they came from.
    fn parsed(query: &str) -> TagParams {
        TagParams::parse(Some(query))
    }

    #[test]
    fn an_empty_or_missing_query_defaults_to_everything_off() {
        for query in ["", "foo=bar&limit=5"] {
            let params = parsed(query);
            assert_eq!(params, TagParams::default(), "`{query}` narrows nothing");
        }
    }

    #[test]
    fn the_tag_arrays_fill_in_both_spellings_and_in_order() {
        let params = parsed("any[]=One&all=two&any=three&none[]=%23Four");
        assert_eq!(
            params.any,
            ["one", "three"],
            "`any` and `any[]` fill the same list, lowercased, in order"
        );
        assert_eq!(params.all, ["two"], "the bare `all` is read too");
        assert_eq!(params.none, ["four"], "`#` is stripped from the values");
    }

    #[test]
    fn tag_values_that_normalize_to_nothing_are_dropped() {
        let params = parsed("any[]=&any[]=%23&any[]=%20%23x%20&all=a");
        assert_eq!(params.any, ["x"], "only the usable name is kept");
        assert_eq!(params.all, ["a"], "the other lists are unaffected");
    }

    #[test]
    fn the_flags_read_pleroma_style_truthy_text() {
        let params = parsed("local=true&remote=1&only_media=TRUE&with_muted=yes");
        assert!(params.local.get(), "`local=true`");
        assert!(params.remote.get(), "`remote=1`");
        assert!(params.only_media.get(), "`only_media` is case-insensitive");
        assert!(!params.with_muted.get(), "`yes` is not true here");
        let off = parsed("local=false&remote=");
        assert!(
            !off.local.get() && !off.remote.get(),
            "`false` and empty are off"
        );
    }

    #[test]
    fn exclude_visibilities_keeps_the_five_stored_spellings() {
        let params = parsed("exclude_visibilities[]=public&exclude_visibilities[]=Public&all=x");
        assert_eq!(
            params.exclude_visibilities,
            vec![Visibility::Public],
            "the verbatim spelling counts, a capitalization does not"
        );
    }

    #[test]
    fn the_path_tag_leads_the_any_list() {
        let query = parsed("any[]=bar").into_timeline(
            "foo".to_owned(),
            None,
            PageParams {
                max_id: None,
                min_id: None,
                since_id: None,
                limit: 20,
                offset: None,
            },
        );
        let TimelineSource::Hashtag { any, all, none } = &query.source else {
            panic!("a tag timeline is a hashtag source");
        };
        let names: Vec<&str> = any.iter().map(String::as_str).collect();
        assert_eq!(names, ["foo", "bar"], "the path tag rides with `any[]`");
        assert!(all.is_empty() && none.is_empty(), "nothing else was asked");
        assert!(query.viewer.is_none(), "an anonymous query has no viewer");
    }

    #[test]
    fn direct_reads_only_with_muted() {
        let on = DirectParams::parse(Some("with_muted=true&limit=5&max_id=abc"));
        assert!(on.with_muted.get(), "`with_muted=true` is on");
        let one = DirectParams::parse(Some("with_muted=1"));
        assert!(one.with_muted.get(), "`with_muted=1` is on too");
        let off = DirectParams::parse(Some("with_muted=yes&with_muted=false"));
        assert!(
            !off.with_muted.get(),
            "the last occurrence wins, `no` is off"
        );
        assert_eq!(
            DirectParams::parse(Some("limit=5&since_id=7")),
            DirectParams::default(),
            "the page keys belong to `PageParams` and are not looked at"
        );
        assert_eq!(
            DirectParams::parse(None),
            DirectParams::default(),
            "no query is everything off"
        );
    }

    #[test]
    fn the_list_reads_only_only_media_and_with_muted() {
        let on = ListParams::parse(Some("only_media=true&with_muted=1&limit=5&max_id=abc"));
        assert!(on.only_media.get(), "`only_media=true` is on");
        assert!(on.with_muted.get(), "`with_muted=1` is on too");
        let off = ListParams::parse(Some("only_media=yes&with_muted=false"));
        assert!(
            !off.only_media.get() && !off.with_muted.get(),
            "`yes` is not true and the last occurrence wins"
        );
        assert_eq!(
            ListParams::parse(Some("local=true&min_id=7")),
            ListParams::default(),
            "foreign keys and the page keys are not looked at"
        );
        assert_eq!(
            ListParams::parse(None),
            ListParams::default(),
            "no query is everything off"
        );
    }

    /// A database-backed [`User`] for [`DirectParams::into_timeline`],
    /// which owns its viewer and so cannot be built from a literal.
    async fn fixture_user() -> (crate::test_db::TestDb, User) {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let alice = crate::models::user::insert(
            db.pool(),
            &crate::models::user::NewUser::local(
                "alice",
                &crate::models::user::local_ap_id(crate::config::TEST_BASE_URL, "alice"),
            ),
        )
        .await
        .expect("the fixture user inserts");
        (db, alice)
    }

    #[tokio::test]
    async fn the_direct_query_is_the_viewers_own_conversations() {
        let (_db, alice) = fixture_user().await;
        let alice_id = alice.id;
        let page = PageParams {
            max_id: None,
            min_id: None,
            since_id: None,
            limit: 7,
            offset: None,
        };
        let query = DirectParams {
            with_muted: Flag::new(true),
        }
        .into_timeline(alice, page);
        assert_eq!(query.source, TimelineSource::Direct, "the direct source");
        assert!(
            query
                .viewer
                .as_ref()
                .is_some_and(|viewer| viewer.id == alice_id),
            "the query owns the authenticated viewer"
        );
        assert_eq!(
            query.filters.with_muted,
            Flag::new(true),
            "the one filter rides from the query string"
        );
        assert_eq!(query.page, page, "the page is threaded through");
    }

    #[tokio::test]
    async fn the_list_query_names_the_owned_list_and_its_two_toggles() {
        let (_db, alice) = fixture_user().await;
        let alice_id = alice.id;
        let list_id = Uuid::now_v7();
        let page = PageParams {
            max_id: None,
            min_id: None,
            since_id: None,
            limit: 20,
            offset: None,
        };
        let query = ListParams {
            only_media: Flag::new(true),
            with_muted: Flag::new(false),
        }
        .into_timeline(list_id, alice, page);
        assert_eq!(
            query.source,
            TimelineSource::List { list_id },
            "the query is built over the proven-owned list"
        );
        assert!(
            query
                .viewer
                .as_ref()
                .is_some_and(|viewer| viewer.id == alice_id),
            "the query owns the authenticated viewer"
        );
        assert_eq!(
            query.filters.only_media,
            Flag::new(true),
            "`only_media` rides from the query string"
        );
        assert_eq!(
            query.filters.with_muted,
            Flag::new(false),
            "`with_muted` defaults to off"
        );
        assert_eq!(query.page, page, "the page is threaded through");
    }

    /// The parsed home query, expecting success: `reply_visibility` is the
    /// one key this parse can reject, and the rejects get their own test.
    fn home_parsed(query: &str) -> HomeParams {
        HomeParams::parse(Some(query)).expect("a well-formed home query parses")
    }

    #[test]
    fn the_home_reads_the_toggles_and_visibilities_like_the_tag_route() {
        let params = home_parsed(
            "local=true&remote=1&only_media=TRUE&with_muted=yes\
             &exclude_visibilities[]=public&exclude_visibilities=Private",
        );
        assert!(params.local.get(), "`local=true`");
        assert!(params.remote.get(), "`remote=1`");
        assert!(params.only_media.get(), "`only_media` is case-insensitive");
        assert!(!params.with_muted.get(), "`yes` is not true here");
        assert_eq!(
            params.exclude_visibilities,
            vec![Visibility::Public],
            "both spellings fill the one list and only a stored name counts"
        );
        let off = home_parsed("local=false&remote=&exclude_replies=true");
        assert!(
            !off.local.get() && !off.remote.get(),
            "`false` and empty are off"
        );
        assert!(off.exclude_visibilities.is_empty(), "nothing is excluded");
        assert_eq!(
            off.reply_visibility, None,
            "`exclude_replies` is not a home parameter and narrows nothing"
        );
    }

    #[test]
    fn the_home_reply_visibility_takes_the_two_spellings_and_the_last_wins() {
        assert_eq!(
            home_parsed("reply_visibility=following").reply_visibility,
            Some(ReplyVisibility::Following),
            "`following` narrows to the viewer's follows"
        );
        assert_eq!(
            home_parsed("reply_visibility=self").reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "`self` narrows to the viewer"
        );
        assert_eq!(
            home_parsed("reply_visibility=following&reply_visibility=self").reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "the last occurrence wins"
        );
    }

    #[test]
    fn an_unknown_home_reply_visibility_is_a_422() {
        for value in ["", "Following", "everyone", "all"] {
            let query = format!("reply_visibility={value}");
            let err = HomeParams::parse(Some(&query))
                .expect_err(&format!("`{value}` is no reply visibility"));
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "`{value}` is a 422, not {err}"
            );
            assert_eq!(
                err.to_string(),
                REPLY_VISIBILITY_INVALID,
                "the message for `{value}`"
            );
        }
    }

    #[test]
    fn the_home_ignores_the_page_keys_and_unrelated_parameters() {
        for query in [
            "",
            "limit=5&max_id=abc&offset=2&min_id=def&since_id=ghi&foo=bar",
        ] {
            assert_eq!(
                home_parsed(query),
                HomeParams::default(),
                "`{query}` narrows nothing"
            );
        }
        assert_eq!(
            HomeParams::parse(None).expect("no query parses"),
            HomeParams::default(),
            "no query is everything off"
        );
    }

    #[tokio::test]
    async fn the_home_query_is_the_viewers_own_following_timeline() {
        let (_db, alice) = fixture_user().await;
        let alice_id = alice.id;
        let page = PageParams {
            max_id: None,
            min_id: None,
            since_id: None,
            limit: 7,
            offset: None,
        };
        let query = HomeParams {
            local: Flag::new(true),
            remote: Flag::new(false),
            only_media: Flag::new(true),
            with_muted: Flag::new(true),
            exclude_visibilities: vec![Visibility::Direct],
            reply_visibility: Some(ReplyVisibility::SelfOnly),
        }
        .into_timeline(alice, page);
        assert_eq!(query.source, TimelineSource::Home, "the home source");
        assert!(
            query
                .viewer
                .as_ref()
                .is_some_and(|viewer| viewer.id == alice_id),
            "the query owns the authenticated viewer"
        );
        assert_eq!(
            query.filters.local_only,
            Flag::new(true),
            "`local` maps onto the author narrowing"
        );
        assert_eq!(
            query.filters.remote_only,
            Flag::new(false),
            "`remote` defaults to off"
        );
        assert_eq!(
            query.filters.only_media,
            Flag::new(true),
            "`only_media` rides from the query string"
        );
        assert_eq!(
            query.filters.with_muted,
            Flag::new(true),
            "`with_muted` rides from the query string"
        );
        assert_eq!(
            query.filters.exclude_visibilities,
            vec![Visibility::Direct],
            "the visibility exclusions carry over"
        );
        assert_eq!(
            query.filters.reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "the reply narrowing carries over"
        );
        assert!(
            !query.filters.exclude_replies.get(),
            "`exclude_replies` was never asked for and stays off"
        );
        assert_eq!(query.page, page, "the page is threaded through");
    }
}
