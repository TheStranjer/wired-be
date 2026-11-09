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
//! Reference: `lib/pleroma/web/mastodon_api/controllers/timeline_controller.ex`
//! (`hashtag/2` and `hashtag_fetching/3`); the ticket body is authoritative.
//! The later timeline routes (home, public, direct, list) join this module
//! and reuse `timeline_response`.

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Uri},
    routing::get,
};
use url::form_urlencoded;

use crate::{
    auth::MaybeAuthUser,
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
        timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    },
    state::AppState,
};

/// The `/api/v1/timelines` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new().route("/timelines/tag/{tag}", get(tag_timeline))
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

/// The shared response shape of every `/api/v1/timelines/*` endpoint:
/// [`fetch`](TimelineQuery::fetch) `query`, [`render_statuses`] the page for
/// its viewer, and answer the newest-first [`StatusEntity`] array with the
/// `Link` header [`page_link`] derives from the request's own `uri` and the
/// fetched rows' ids (Pleroma's `add_link_headers`). An empty page carries
/// no `Link` header at all.
///
/// The home, public, direct and list routes the later tickets add build
/// their [`TimelineQuery`] the same way [`tag_timeline`] does and hand it
/// here; the rendering, cursors and headers never get restated.
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
}
