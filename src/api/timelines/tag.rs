//! `GET /api/v1/timelines/tag/{tag}`: the hashtag timeline and the
//! request parameters it reads beyond the path tag.

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use url::form_urlencoded;

use crate::{
    api::helpers::{is_true, normalize_tag, push_tag, push_visibility, timeline_response},
    auth::MaybeAuthUser,
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    models::{
        status::Visibility,
        user::{Flag, User},
    },
    oauth::scopes::READ_STATUSES,
    pagination::PageParams,
    services::timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    state::AppState,
};

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
pub(super) async fn tag_timeline(
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

/// The most tags each of `any`, `all` and `none` keeps, Mastodon's
/// `TagFeed::LIMIT_PER_MODE`. Later values are dropped silently, so a request
/// cannot grow the statement by one subquery per query-string key.
const TAG_MODE_LIMIT: usize = 4;

/// [`push_tag`] unless `list` already holds [`TAG_MODE_LIMIT`] names.
fn push_capped_tag(list: &mut Vec<String>, raw: &str) {
    if list.len() < TAG_MODE_LIMIT {
        push_tag(list, raw);
    }
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
                "any" | "any[]" => push_capped_tag(&mut found.any, &value),
                "all" | "all[]" => push_capped_tag(&mut found.all, &value),
                "none" | "none[]" => push_capped_tag(&mut found.none, &value),
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
    fn each_tag_list_keeps_only_the_first_four() {
        for key in ["any", "all", "none"] {
            for spelling in [format!("{key}[]"), key.to_owned()] {
                let query: Vec<String> = ('a'..='z').map(|c| format!("{spelling}={c}")).collect();
                let params = parsed(&query.join("&"));
                let list = match key {
                    "any" => &params.any,
                    "all" => &params.all,
                    _ => &params.none,
                };
                assert_eq!(list, &["a", "b", "c", "d"], "`{spelling}` keeps four");
            }
        }
    }

    #[test]
    fn dropped_values_do_not_count_toward_the_cap() {
        let params = parsed("any[]=&any[]=a&any[]=%23&any[]=b&any[]=c&any[]=d&any[]=e");
        assert_eq!(params.any, ["a", "b", "c", "d"], "empty names take no slot");
    }

    #[test]
    fn the_path_tag_rides_on_top_of_a_full_any_list() {
        let query = parsed("any[]=a&any[]=b&any[]=c&any[]=d&any[]=e").into_timeline(
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
        let TimelineSource::Hashtag { any, .. } = &query.source else {
            panic!("a tag timeline is a hashtag source");
        };
        assert_eq!(
            any.len(),
            TAG_MODE_LIMIT + 1,
            "four `any[]` plus the path tag"
        );
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
        assert!(params.with_muted.get(), "`yes` is a true spelling");
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
}
