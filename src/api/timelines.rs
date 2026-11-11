//! The `/api/v1/timelines/*` status timelines, each one the
//! [`TimelineQuery`] of [`crate::services::timeline`] rendered as a page.
//!
//! The endpoints share one helper, `timeline_response`, which fetches,
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
//! Reference: `lib/pleroma/web/mastodon_api/controllers/timeline_controller.ex`
//! (`hashtag/2`, `hashtag_fetching/3`, `public/2` and
//! `restrict_unauthenticated?/1`) and
//! `lib/pleroma/web/plugs/ensure_public_or_authenticated_plug.ex`. The later
//! timeline routes (home, direct, list) join this module and reuse
//! `timeline_response`.

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Uri},
    routing::get,
};
use url::form_urlencoded;

use crate::{
    auth::MaybeAuthUser,
    config::{Config, RESTRICT_TIMELINES_FEDERATED, RESTRICT_TIMELINES_LOCAL},
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    models::{
        status::{Status as StatusRow, Visibility},
        user::{Flag, User},
    },
    oauth::scopes::READ_STATUSES,
    pagination::{PageParams, link_header, set_link_header},
    services::{
        render_status::render_statuses,
        timeline::{ReplyVisibility, TimelineFilters, TimelineQuery, TimelineSource},
    },
    state::AppState,
};

/// The 403 body a `RESTRICT_UNAUTHENTICATED` gate answers an
/// unauthenticated timeline read with.
///
/// The ticket's exact wording; Pleroma's own refusal is the 401
/// "authorization required for timeline view" (`fail_on_bad_auth`).
const AUTHENTICATION_REQUIRED: &str = "Authentication is required to access this resource.";

/// The `/api/v1/timelines` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/timelines/public", get(public_timeline))
        .route("/timelines/tag/{tag}", get(tag_timeline))
}

/// `GET /api/v1/timelines/tag/{tag}` — the `public` statuses carrying `tag`,
/// newest first, one [`PageParams`] keyset page at a time. Auth is optional
/// (Pleroma's `read:statuses` scopes plug with
/// `fallback: :proceed_unauthenticated`): a presented user token must grant
/// the scope, an app-only token and no token at all both read anonymously,
/// and an invalid token is the extractor's 401.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the presented user token lacks `read:statuses`; [`AppError::NotFound`]
/// for a path tag that normalizes to nothing; [`AppError::BadRequest`] from
/// [`PageParams`] for a malformed cursor. Any database failure arrives
/// through `?` as [`AppError::Database`], and [`set_link_header`] reports a
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
    /// [`Uri::query`] returns, percent-decoding and all). Every array key
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

/// `GET /api/v1/timelines/public` — the instance's public page.
///
/// The [`TimelineSource::Public`] rows (Pleroma's `fetch_public_activities`),
/// newest first, one [`PageParams`] keyset page at a time. Auth is optional
/// (Pleroma's `read:statuses` scopes plug with
/// `fallback: :proceed_unauthenticated`): a presented user token must grant
/// the scope, an app-only token and no token at all both read anonymously,
/// and an invalid token is the extractor's 401 — all before the
/// [`PublicParams::ensure_open`] restriction gate runs.
///
/// The `Link` header's path is the request's own [`OriginalUri`], as in
/// [`tag_timeline`].
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the presented user token lacks `read:statuses`, and from
/// [`PublicParams::ensure_open`] when the instance restricts this
/// unauthenticated read; [`AppError::BadRequest`] from [`PageParams`] for a
/// malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn public_timeline(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let params = PublicParams::parse(uri.query());
    params.ensure_open(&state.config, auth.0.is_some())?;
    let query = params.into_timeline(auth.0, page);
    timeline_response(&state, &query, &uri).await
}

/// The public timeline's request parameters.
///
/// The origin/media/mute narrowings and visibility exclusions the tag route
/// shares, plus Pleroma's two public-page extensions, `reply_visibility`
/// and `instance` — every one of them a no-op in its [`Default`] spelling.
/// The booleans are [`Flag`]s because that is what this repository models
/// every boolean with (see [`crate::models::user`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PublicParams {
    /// `local`: keep only statuses this instance authored.
    ///
    /// It is also the parameter that picks which [`Config::restricts`] area
    /// gates an anonymous read (see [`RESTRICT_TIMELINES_LOCAL`]).
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
    /// `reply_visibility`: keep only replies addressed to the viewer
    /// (`self`) or to the viewer and their follows (`following`); any other
    /// spelling — or no value — keeps every reply.
    reply_visibility: Option<ReplyVisibility>,
    /// `instance`: keep only statuses whose author is a remote account of
    /// this domain (Pleroma's author-domain extension); `None` filters
    /// nothing.
    instance: Option<String>,
}

impl PublicParams {
    /// Reads the parameters out of a raw query string the way
    /// [`TagParams::parse`] reads its own.
    ///
    /// Percent-decoded; `key` and `key[]` fill the
    /// [`Self::exclude_visibilities`] list in occurrence order; a repeated
    /// single-value key takes its last value; an unrelated key is never
    /// looked at.
    fn parse(query: Option<&str>) -> Self {
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
                    found.reply_visibility = parse_reply_visibility(&value);
                }
                "instance" => found.instance = parse_instance(&value),
                _ => {}
            }
        }
        found
    }

    /// Pleroma's `restrict_unauthenticated?` gate, in
    /// `timeline_controller.ex`.
    ///
    /// With no user behind the request, the `local=true` page belongs to
    /// the `timelines_local` area and every other read to
    /// `timelines_federated`, and a listed area refuses the request
    /// [`AUTHENTICATION_REQUIRED`]. An authenticated request (`authenticated`
    /// — a live *user* token was presented; an app-only one is
    /// unauthenticated here, as in Pleroma's `conn.assigns[:user]`) and
    /// every unlisted area pass.
    ///
    /// # Errors
    ///
    /// [`AppError::Forbidden`] with [`AUTHENTICATION_REQUIRED`] when the
    /// anonymous read of this page's area is restricted.
    fn ensure_open(&self, config: &Config, authenticated: bool) -> AppResult<()> {
        if authenticated {
            return Ok(());
        }
        let area = if self.local.get() {
            RESTRICT_TIMELINES_LOCAL
        } else {
            RESTRICT_TIMELINES_FEDERATED
        };
        if config.restricts(area) {
            return Err(AppError::Forbidden(AUTHENTICATION_REQUIRED.to_owned()));
        }
        Ok(())
    }

    /// Assembles the [`TimelineQuery`] the request asks for.
    ///
    /// The [`TimelineSource::Public`] page, narrowed by exactly these
    /// [`TimelineFilters`], as `viewer` (or anonymously, `None`).
    fn into_timeline(self, viewer: Option<User>, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer,
            source: TimelineSource::Public,
            filters: TimelineFilters {
                local_only: self.local,
                remote_only: self.remote,
                only_media: self.only_media,
                with_muted: self.with_muted,
                exclude_visibilities: self.exclude_visibilities,
                reply_visibility: self.reply_visibility,
                instance: self.instance,
                ..TimelineFilters::default()
            },
            page,
        }
    }
}

/// The shared response shape of every `/api/v1/timelines/*` endpoint:
/// [`fetch`](TimelineQuery::fetch) `query`, [`render_statuses`] the page for
/// its viewer, and answer the newest-first [`StatusEntity`] array with the
/// `Link` header [`page_link`] derives from the request's own `uri` and the
/// fetched rows' ids (Pleroma's `add_link_headers`). An empty page carries
/// no `Link` header at all.
///
/// The home, direct and list routes the later tickets add build their
/// [`TimelineQuery`] the same way [`tag_timeline`] and [`public_timeline`]
/// do and hand it here; the rendering, cursors and headers never get
/// restated.
async fn timeline_response(
    state: &AppState,
    query: &TimelineQuery,
    uri: &Uri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    let rows = query.fetch(&state.db).await?;
    let link = page_link(state, uri, &rows);
    let statuses = render_statuses(state, query.viewer.as_ref(), &rows).await?;
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    Ok((headers, Json(statuses)))
}

/// The `Link` header value for one fetched page: [`link_header`] over the
/// first (newest) and last (oldest) row's id — the same
/// [`add_link_headers`](crate::pagination::link_header) cursors every other
/// listing here writes — with the request's own parameters repeated per
/// Pleroma's `build_pagination_fields`, and `None` for an empty page.
fn page_link(state: &AppState, uri: &Uri, rows: &[StatusRow]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(
        &state.config.base_url,
        uri.path(),
        &link_query(uri.query()),
        &newest,
        &oldest,
    )
}

/// The query parameters a page's `Link` targets repeat: the request's own
/// pairs with the four cursors [`PageParams`] reads ([`CURSOR_KEYS`])
/// dropped — each link carries its own, and the next page starts where this
/// one ended. Every other parameter, `limit` included, rides on both links
/// exactly as it arrived, so a followed link re-asks the same filtered
/// timeline.
fn link_query(query: Option<&str>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .filter(|(key, _value)| !CURSOR_KEYS.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// The pagination keys the `Link` header replaces with its own cursors: the
/// three id cursors and `offset`, the cursors [`PageParams`] reads
/// (`limit` is a page size, not a cursor, and stays).
const CURSOR_KEYS: [&str; 4] = ["max_id", "min_id", "since_id", "offset"];

/// A tag name as the request writes it, in the spelling the `hashtags`
/// column stores: trimmed, leading `#`s dropped, lowercased (see
/// [`crate::models::hashtag::normalize_name`], from which this differs only
/// in not capping the length — a name over the storage bound can be no
/// stored tag, and the timeline answers such a lookup the empty page rather
/// than an error). `None` when nothing usable is left: the endpoint's 404.
fn normalize_tag(raw: &str) -> Option<String> {
    let stripped = raw.trim().trim_start_matches('#').trim();
    (!stripped.is_empty()).then(|| stripped.to_lowercase())
}

/// Appends `raw` to `list` as [`normalize_tag`] spells it; a name that
/// normalizes to nothing is dropped, Pleroma's `Enum.filter(& &1)` — an
/// empty `any[]` entry widens the match with nothing.
fn push_tag(list: &mut Vec<String>, raw: &str) {
    if let Some(tag) = normalize_tag(raw) {
        list.push(tag);
    }
}

/// Appends `raw` to `list` when it spells one of the five stored
/// visibilities ([`Visibility`]'s `FromStr`, the verbatim lowercase
/// spellings clients send); anything else is ignored, narrowing nothing.
fn push_visibility(list: &mut Vec<Visibility>, raw: &str) {
    if let Ok(visibility) = raw.trim().parse::<Visibility>() {
        list.push(visibility);
    }
}

/// Whether a query parameter's text spells a true boolean: `true` or `1`,
/// trimmed and matched case-insensitively — the same reading
/// [`crate::api::search`] gives its flags; anything else (absent, empty,
/// `false`, junk) is `false`.
fn is_true(text: &str) -> bool {
    let text = text.trim();
    text.eq_ignore_ascii_case("true") || text == "1"
}

/// The [`ReplyVisibility`] a `reply_visibility` parameter names.
///
/// Pleroma's two values `following` and `self`, trimmed and spelled exactly
/// (the way [`push_visibility`] takes only the verbatim visibility names);
/// anything else — `all`, `ignored`, junk — keeps every reply, which is the
/// parameter's absence.
fn parse_reply_visibility(text: &str) -> Option<ReplyVisibility> {
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
    fn normalize_tag_lowercases_strips_hash_and_rejects_nothing_usable() {
        assert_eq!(normalize_tag("#Foo").as_deref(), Some("foo"));
        assert_eq!(normalize_tag("##Bar ").as_deref(), Some("bar"));
        assert_eq!(normalize_tag(" Café ").as_deref(), Some("café"));
        assert_eq!(normalize_tag("#").as_deref(), None, "a lone `#` is empty");
        assert_eq!(
            normalize_tag("  #  ").as_deref(),
            None,
            "whitespace is empty"
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
    fn the_link_repeats_the_filters_and_drops_the_cursors() {
        let extras = link_query(Some(
            "any[]=a&local=1&limit=2&max_id=old&min_id=new&since_id=s&offset=3",
        ));
        assert_eq!(
            extras,
            vec![
                ("any[]".to_owned(), "a".to_owned()),
                ("local".to_owned(), "1".to_owned()),
                ("limit".to_owned(), "2".to_owned()),
            ],
            "filters and `limit` ride on, the cursors and `offset` do not"
        );
        assert!(link_query(None).is_empty(), "no query repeats nothing");
    }

    /// The parsed public-page query as [`PublicParams`].
    fn public_parsed(query: &str) -> PublicParams {
        PublicParams::parse(Some(query))
    }

    /// A [`Config`] whose `RESTRICT_UNAUTHENTICATED` lists `areas`.
    fn restricted(areas: &[&str]) -> Config {
        let mut config = Config::for_tests();
        config.restrict_unauthenticated = areas.iter().map(|area| (*area).to_owned()).collect();
        config
    }

    #[test]
    fn an_empty_public_query_defaults_to_everything_off() {
        for query in ["", "foo=bar&limit=5"] {
            assert_eq!(
                public_parsed(query),
                PublicParams::default(),
                "`{query}` narrows nothing"
            );
        }
    }

    #[test]
    fn the_public_flags_and_exclusions_read_like_the_tags_ones() {
        let params = public_parsed(
            "local=true&remote=1&only_media=TRUE&with_muted=true&exclude_visibilities[]=private",
        );
        assert!(params.local.get(), "`local=true`");
        assert!(params.remote.get(), "`remote=1`");
        assert!(params.only_media.get(), "`only_media` is case-insensitive");
        assert!(params.with_muted.get(), "`with_muted=true`");
        assert_eq!(
            params.exclude_visibilities,
            vec![Visibility::Private],
            "the verbatim visibility name is kept"
        );
        let off = public_parsed("local=false&remote=&exclude_visibilities[]=Public");
        assert!(
            !off.local.get() && !off.remote.get() && off.exclude_visibilities.is_empty(),
            "`false`, empty and un-spelled values narrow nothing"
        );
    }

    #[test]
    fn reply_visibility_names_the_two_filters_and_shrugs_at_the_rest() {
        assert_eq!(
            public_parsed("reply_visibility=following").reply_visibility,
            Some(ReplyVisibility::Following),
            "`following` is Pleroma's friends-and-self filter"
        );
        assert_eq!(
            public_parsed("reply_visibility=%20self%20").reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "`self` is trimmed like every other value"
        );
        for value in ["all", "ignored", "Self", ""] {
            assert_eq!(
                public_parsed(&format!("reply_visibility={value}")).reply_visibility,
                None,
                "`{value}` is no filter and keeps every reply"
            );
        }
    }

    #[test]
    fn instance_is_trimmed_and_an_empty_one_filters_nothing() {
        assert_eq!(
            public_parsed("instance=remote.example").instance.as_deref(),
            Some("remote.example"),
            "the domain is kept as spelled"
        );
        assert_eq!(
            public_parsed("instance=%20cdn.example%20")
                .instance
                .as_deref(),
            Some("cdn.example"),
            "whitespace trims off"
        );
        assert_eq!(
            public_parsed("instance=").instance,
            None,
            "an empty domain is no filter at all"
        );
        assert_eq!(
            public_parsed("instance=%20%20%20").instance,
            None,
            "and a blank one reads the same"
        );
    }

    #[test]
    fn the_public_page_maps_to_the_public_source_with_its_filters() {
        let query =
            public_parsed("local=true&only_media=1&reply_visibility=self&instance=remote.example")
                .into_timeline(
                    None,
                    PageParams {
                        max_id: None,
                        min_id: None,
                        since_id: None,
                        limit: 20,
                        offset: None,
                    },
                );
        assert_eq!(query.source, TimelineSource::Public, "the public page");
        assert!(query.filters.local_only.get(), "`local` rides along");
        assert!(query.filters.only_media.get(), "`only_media` rides along");
        assert_eq!(
            query.filters.reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "`reply_visibility` rides along"
        );
        assert_eq!(
            query.filters.instance.as_deref(),
            Some("remote.example"),
            "`instance` rides along"
        );
        assert!(query.viewer.is_none(), "an anonymous query has no viewer");
    }

    #[test]
    fn the_restriction_gate_leaves_every_open_read_through() {
        let open = Config::for_tests();
        let local = public_parsed("local=true");
        let federated = public_parsed("");
        local
            .ensure_open(&open, false)
            .expect("an open instance answers an anonymous `local=true`");
        federated
            .ensure_open(&open, false)
            .expect("an open instance answers an anonymous federated read");
        federated
            .ensure_open(&open, true)
            .expect("an authenticated read was never the gate's business");
    }

    #[test]
    fn timelines_local_restricted_refuses_only_the_anonymous_local_read() {
        use axum::{http::StatusCode, response::IntoResponse as _};

        let config = restricted(&[RESTRICT_TIMELINES_LOCAL]);
        let refusal = public_parsed("local=true")
            .ensure_open(&config, false)
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
        public_parsed("local=false")
            .ensure_open(&config, false)
            .expect("a `local=false` read is the federated page, and that one is open");
        public_parsed("")
            .ensure_open(&config, false)
            .expect("the plain federated read stays open");
        public_parsed("local=true")
            .ensure_open(&config, true)
            .expect("an authenticated `local=true` read passes");
    }

    #[test]
    fn timelines_federated_restricted_refuses_every_other_anonymous_read() {
        let config = restricted(&[RESTRICT_TIMELINES_FEDERATED]);
        let refusal = public_parsed("")
            .ensure_open(&config, false)
            .expect_err("the anonymous federated read is refused");
        assert_eq!(
            refusal.to_string(),
            AUTHENTICATION_REQUIRED,
            "the refusal is the same wording"
        );
        let remote_refusal = public_parsed("remote=true")
            .ensure_open(&config, false)
            .expect_err("`remote=true` is still the federated page");
        assert_eq!(
            remote_refusal.to_string(),
            AUTHENTICATION_REQUIRED,
            "`remote=true` is the federated page and shares its refusal"
        );
        public_parsed("local=true")
            .ensure_open(&config, false)
            .expect("the `local=true` page belongs to the other area and stays open");
        public_parsed("")
            .ensure_open(&config, true)
            .expect("an authenticated federated read passes");
    }

    #[test]
    fn both_areas_restricted_refuse_the_whole_anonymous_timeline() {
        let config = restricted(&[RESTRICT_TIMELINES_LOCAL, RESTRICT_TIMELINES_FEDERATED]);
        for query in ["", "local=true", "local=false&remote=true"] {
            let refusal = public_parsed(query)
                .ensure_open(&config, false)
                .expect_err("a listed area refuses every anonymous read");
            assert_eq!(
                refusal.to_string(),
                AUTHENTICATION_REQUIRED,
                "both areas listed, every anonymous read is the same refusal"
            );
        }
        public_parsed("local=true")
            .ensure_open(&config, true)
            .expect("and an authenticated read passes whatever is listed");
    }
}
