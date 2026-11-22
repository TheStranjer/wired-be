//! `GET /api/v1/timelines/direct` and `GET /api/v1/timelines/list/{list_id}`:
//! the two timelines that need a signed-in viewer, with the request
//! parameters each reads.

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    api::helpers::{is_true, timeline_response},
    auth::AuthUser,
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    models::{
        list,
        user::{Flag, User},
    },
    oauth::scopes::{READ_LISTS, READ_STATUSES},
    pagination::PageParams,
    services::timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    state::AppState,
    util::id::parse_id,
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
/// [`AppError::Forbidden`] from [`AuthUser::require`] when the token lacks
/// `read:statuses`; [`AppError::BadRequest`] from [`PageParams`] for a
/// malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
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
pub(super) async fn list_timeline(
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

#[cfg(test)]
mod tests {
    use super::super::test_support::fixture_user;
    use super::*;

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
}
