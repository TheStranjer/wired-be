//! `GET /api/v1/timelines/direct` and `GET /api/v1/timelines/list/{list_id}`:
//! the two timelines that need a signed-in viewer, with the request
//! parameters each reads.

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use uuid::Uuid;

use super::common_filters;
use crate::{
    api::{
        helpers::{QueryPairs, timeline_response},
        lists::owned_list,
    },
    auth::AuthUser,
    entities::status::Status as StatusEntity,
    error::AppResult,
    models::user::User,
    oauth::scopes::{READ_LISTS, READ_STATUSES},
    pagination::PageParams,
    services::timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    state::AppState,
};

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
/// [`AppError::Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser::require`] when the token lacks
/// `read:statuses`; [`AppError::BadRequest`](crate::error::AppError::BadRequest) from [`PageParams`] for a
/// malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError::Database), and [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value
/// that is not header-safe as [`AppError::Internal`](crate::error::AppError::Internal).
pub(super) async fn direct_timeline(
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
/// besides the cursors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DirectParams {
    /// The filters this route honours: only `with_muted` (skip the viewer's
    /// block/mute exclusions, Pleroma's `with_muted`, default `false`) is
    /// kept out of [`common_filters`]; every other field is cleared.
    filters: TimelineFilters,
}

impl DirectParams {
    /// Reads the parameters out of a raw query string (what `Uri::query`
    /// returns, percent-decoding and all); the cursors and `limit` are
    /// [`PageParams`]'s to read and every other key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let common = common_filters(&QueryPairs::parse(query));
        Self {
            filters: TimelineFilters {
                with_muted: common.with_muted,
                ..TimelineFilters::default()
            },
        }
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::Direct`] as `viewer`, who is always present
    /// because the route requires authentication, with only the
    /// `with_muted` filter set from the query.
    fn into_timeline(self, viewer: User, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer: Some(viewer),
            source: TimelineSource::Direct,
            filters: self.filters,
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
/// [`AppError::Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser::require`] when the token lacks
/// `read:lists`; [`AppError::NotFound`](crate::error::AppError::NotFound) when `list_id` is malformed, names
/// no list or names another account's (see [`owned_list`], Pleroma
/// answering a foreign `fetch_activities` list with its generic 404);
/// [`AppError::BadRequest`](crate::error::AppError::BadRequest) from [`PageParams`] for a malformed cursor.
/// Any database failure arrives through `?` as [`AppError::Database`](crate::error::AppError::Database), and
/// [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value that is not header-safe as
/// [`AppError::Internal`](crate::error::AppError::Internal).
pub(super) async fn list_timeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(raw_id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require(READ_LISTS)?;
    let list_id = owned_list(&state, auth.0.id, &raw_id).await?;
    let query = ListParams::parse(uri.query()).into_timeline(list_id, auth.0, page);
    timeline_response(&state, &query, &uri).await
}

/// The list timeline's request parameters beyond the [`PageParams`] page
/// and the path id: the two toggles the ticket names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ListParams {
    /// The filters this route honours: `only_media` (keep only statuses
    /// carrying an attachment) and `with_muted` (skip the viewer's
    /// block/mute exclusions, Pleroma's `with_muted`, default `false`) are
    /// kept out of [`common_filters`]; every other field is cleared.
    filters: TimelineFilters,
}

impl ListParams {
    /// Reads the parameters out of a raw query string (what `Uri::query`
    /// returns, percent-decoding and all); the cursors and `limit` are
    /// [`PageParams`]'s to read and every other key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let common = common_filters(&QueryPairs::parse(query));
        Self {
            filters: TimelineFilters {
                only_media: common.only_media,
                with_muted: common.with_muted,
                ..TimelineFilters::default()
            },
        }
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::List`] of `list_id` as `viewer` — who is always
    /// present because the route requires authentication, and whose list
    /// `list_id` names because [`owned_list`] proved it — with the two
    /// toggles set from the query.
    fn into_timeline(self, list_id: Uuid, viewer: User, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer: Some(viewer),
            source: TimelineSource::List { list_id },
            filters: self.filters,
            page,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::fixture_user;
    use super::*;
    use crate::models::user::Flag;

    #[test]
    fn direct_reads_only_with_muted() {
        let on = DirectParams::parse(Some("with_muted=true&limit=5&max_id=abc"));
        assert!(on.filters.with_muted.get(), "`with_muted=true` is on");
        let one = DirectParams::parse(Some("with_muted=1"));
        assert!(one.filters.with_muted.get(), "`with_muted=1` is on too");
        let off = DirectParams::parse(Some("with_muted=yes&with_muted=false"));
        assert!(
            !off.filters.with_muted.get(),
            "the last occurrence wins, `false` is off"
        );
        assert_eq!(
            DirectParams::parse(Some(
                "local=true&remote=true&only_media=true&exclude_visibilities[]=public"
            )),
            DirectParams::default(),
            "the shared keys the direct timeline does not honour are cleared"
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
        assert!(on.filters.only_media.get(), "`only_media=true` is on");
        assert!(on.filters.with_muted.get(), "`with_muted=1` is on too");
        let off = ListParams::parse(Some("only_media=maybe&with_muted=true&with_muted=false"));
        assert!(
            !off.filters.only_media.get() && !off.filters.with_muted.get(),
            "junk is not true and the last occurrence wins"
        );
        let yes = ListParams::parse(Some("only_media=yes"));
        assert!(yes.filters.only_media.get(), "`yes` is a true spelling");
        assert_eq!(
            ListParams::parse(Some(
                "local=true&remote=true&exclude_visibilities[]=public&min_id=7"
            )),
            ListParams::default(),
            "foreign keys, the unhonoured shared ones and the page keys are not looked at"
        );
        assert_eq!(
            ListParams::parse(None),
            ListParams::default(),
            "no query is everything off"
        );
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
            filters: TimelineFilters {
                with_muted: Flag::new(true),
                ..TimelineFilters::default()
            },
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
            filters: TimelineFilters {
                only_media: Flag::new(true),
                with_muted: Flag::new(false),
                ..TimelineFilters::default()
            },
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
}
