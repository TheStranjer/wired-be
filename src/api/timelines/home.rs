//! `GET /api/v1/timelines/home`: the viewer's following timeline and the
//! request parameters it reads.

use axum::{
    Json,
    extract::{OriginalUri, State},
    http::HeaderMap,
};
use url::form_urlencoded;

use super::{common_filters, parse_reply_visibility};
use crate::{
    api::helpers::{QueryPairs, timeline_response},
    auth::AuthUser,
    entities::status::Status as StatusEntity,
    error::AppResult,
    models::user::User,
    oauth::scopes::READ_STATUSES,
    pagination::PageParams,
    services::timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    state::AppState,
};

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
/// [`AppError::Forbidden`](crate::error::AppError::Forbidden) from
/// [`AuthUser::require`] when the token lacks
/// `read:statuses`; [`AppError::Unprocessable`](crate::error::AppError::Unprocessable)
/// from [`HomeParams::parse`]
/// for a `reply_visibility` outside `following` and `self`;
/// [`AppError::BadRequest`](crate::error::AppError::BadRequest) from [`PageParams`] for a malformed cursor.
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError::Database), and
/// [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value that is not header-safe as
/// [`AppError::Internal`](crate::error::AppError::Internal).
pub(super) async fn home_timeline(
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
/// every one of them a no-op in its [`Default`] spelling.
/// `exclude_replies` is not a parameter of this timeline (Pleroma's
/// `home_operation` does not list it), so [`HomeParams::parse`] never
/// reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HomeParams {
    /// The narrowings [`common_filters`] reads plus `reply_visibility`:
    /// which replies survive; `None` keeps them all, anything outside
    /// `following` and `self` is the parse's 422.
    filters: TimelineFilters,
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
    /// [`AppError::Unprocessable`](crate::error::AppError::Unprocessable) from
    /// [`parse_reply_visibility`].
    fn parse(query: Option<&str>) -> AppResult<Self> {
        let mut reply_visibility = None;
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            if key == "reply_visibility" {
                reply_visibility = Some(parse_reply_visibility(&value)?);
            }
        }
        Ok(Self {
            filters: TimelineFilters {
                reply_visibility,
                ..common_filters(&QueryPairs::parse(query))
            },
        })
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::Home`] as `viewer` — who is always present
    /// because the route requires authentication — with the toggles, the
    /// visibility exclusions and the reply narrowing set from the query.
    fn into_timeline(self, viewer: User, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer: Some(viewer),
            source: TimelineSource::Home,
            filters: self.filters,
            page,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{REPLY_VISIBILITY_INVALID, test_support::fixture_user};
    use super::*;
    use crate::{
        error::AppError,
        models::{status::Visibility, user::Flag},
        services::timeline::ReplyVisibility,
    };

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
        )
        .filters;
        assert!(params.local_only.get(), "`local=true`");
        assert!(params.remote_only.get(), "`remote=1`");
        assert!(params.only_media.get(), "`only_media` is case-insensitive");
        assert!(params.with_muted.get(), "`yes` is a true spelling");
        assert_eq!(
            params.exclude_visibilities,
            vec![Visibility::Public],
            "both spellings fill the one list and only a stored name counts"
        );
        let off = home_parsed("local=false&remote=&exclude_replies=true").filters;
        assert!(
            !off.local_only.get() && !off.remote_only.get(),
            "`false` and empty are off"
        );
        assert!(off.exclude_visibilities.is_empty(), "nothing is excluded");
        assert_eq!(
            off.reply_visibility, None,
            "`exclude_replies` is not a home parameter and narrows nothing"
        );
        assert!(!off.exclude_replies.get(), "and it stays off");
    }

    #[test]
    fn the_home_reply_visibility_takes_the_two_spellings_and_the_last_wins() {
        assert_eq!(
            home_parsed("reply_visibility=following")
                .filters
                .reply_visibility,
            Some(ReplyVisibility::Following),
            "`following` narrows to the viewer's follows"
        );
        assert_eq!(
            home_parsed("reply_visibility=self")
                .filters
                .reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "`self` narrows to the viewer"
        );
        assert_eq!(
            home_parsed("reply_visibility=following&reply_visibility=self")
                .filters
                .reply_visibility,
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
            filters: TimelineFilters {
                local_only: Flag::new(true),
                remote_only: Flag::new(false),
                only_media: Flag::new(true),
                with_muted: Flag::new(true),
                exclude_visibilities: vec![Visibility::Direct],
                reply_visibility: Some(ReplyVisibility::SelfOnly),
                ..TimelineFilters::default()
            },
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
