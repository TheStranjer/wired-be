//! `GET /api/v1/accounts/{id}/statuses`: the account's posting history
//! read through the shared timeline builder, narrowed by
//! [`StatusesParams`], with the `pinned` branch answered by
//! [`pinned_statuses`].
//!
//! [`pinned_statuses`]: super::pinned_statuses
//!
//! `statuses` is the account's posting history, Pleroma's `.statuses` over
//! `ActivityPub.fetch_user_activities/3`: the same [`MaybeAuthUser`] gate
//! with `read:statuses` instead, the same [`PageParams`] page, and the rows
//! of [`TimelineSource::Account`] read through the shared
//! [`crate::services::timeline`] builder and rendered by the same helper the
//! `/api/v1/timelines/*` routes answer with (see
//! [`timeline_response`]), so the
//! page is the account's own posts *and its reblogs* — each row still judged
//! by the viewer's visibility, and the viewer's own exclusions unless
//! `with_muted`. `pinned`, `only_media`, `exclude_replies`,
//! `exclude_reblogs`, `tagged` and `exclude_visibilities[]` narrow it as
//! Pleroma's `statuses_operation` reads them, `pinned` also taking the page
//! off the pagination keys and dropping the `Link` header with it. A
//! malformed, unknown or deactivated id is the shared 404; a viewer the
//! account blocked sees the empty list; and while `RESTRICT_UNAUTHENTICATED`
//! names the account's profile half (`profiles_local` for a local account,
//! `profiles_remote` for a mirror) an unauthenticated request is the 403
//! `{"error":"This API requires an authenticated user"}` — see
//! [`Config::restricts`].
//!

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    api::helpers::{account_or_404, is_true, normalize_tag, push_visibility, timeline_response},
    auth::MaybeAuthUser,
    entities::status::Status as StatusEntity,
    error::AppResult,
    models::{
        status::Visibility,
        user::{Flag, User},
    },
    oauth::scopes::READ_STATUSES,
    pagination::PageParams,
    services::timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    state::AppState,
};

use super::{
    follow_lists::require_profile_read,
    pinned_statuses::{every_pin, pinned_response},
};

/// `GET /api/v1/accounts/{id}/statuses` — one account's posting history as
/// a page of [`StatusEntity`] rows, newest first: the statuses it authored
/// and its own reblogs — Pleroma's `statuses` action over
/// `ActivityPub.fetch_user_activities/3`, which reads `Create` and
/// `Announce` alike, so `exclude_reblogs` (not Mastodon's default-off
/// `with_reblogs`) is what takes the boosts out. Auth is optional
/// ([`MaybeAuthUser`] with `read:statuses` enforced on a presented user
/// token, Pleroma's scopes plug with `fallback: :proceed_unauthenticated`),
/// and the page every viewer reaches is judged row by row by
/// [`push_visibility_clause`]: a stranger (or an anonymous request) sees
/// `public` and `unlisted`, a follower adds `private`, a mentioned party
/// adds `direct`, and the account behind the token sees everything of its
/// own. An account that blocked the viewer answers the empty list — the
/// visibility clause's rule, not a check of its own — and a malformed,
/// unknown or deactivated id is the shared 404.
///
/// The read is [`TimelineSource::Account`] over the shared
/// [`crate::services::timeline`] builder, narrowed by the request's filters
/// ([`StatusesParams`]) and answered by [`timeline_response`], so its
/// `Link` header repeats the filters and drops the cursors exactly as the
/// `/api/v1/timelines/*` routes do. `pinned=true` is the one exception: it
/// ignores the pagination keys, answers the account's whole pinned set in
/// the order its profile features it (newest pin first) and sends no `Link`
/// header at all.
///
/// While `RESTRICT_UNAUTHENTICATED` names the account's profile area —
/// `profiles_local` for this instance's own, `profiles_remote` for a mirror
/// — an unauthenticated request is the 403 [`USER_REQUIRED`]; a presented
/// user token is never refused for it (see [`require_profile_read`]).
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the authenticated token lacks `read:statuses`, and from
/// [`require_profile_read`] for a restricted area behind an anonymous
/// request; [`AppError::Unauthorized`] from the extractor for an invalid
/// token; [`AppError::NotFound`] for a malformed, unknown or deactivated
/// account id; [`AppError::BadRequest`] from [`PageParams`] for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
///
/// [`push_visibility_clause`]: crate::models::status_visibility::push_visibility_clause
/// [`set_link_header`]: crate::pagination::set_link_header
pub(super) async fn list_statuses(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0;
    require_profile_read(&state.config, viewer.as_ref(), &target)?;
    let query = StatusesParams::parse(uri.query()).into_timeline(target.id, viewer, page);
    if query.filters.pinned_only.get() {
        pinned_response(&state, &query, target.id).await
    } else {
        timeline_response(&state, &query, &uri).await
    }
}

/// The non-cursor query parameters of `GET /api/v1/accounts/{id}/statuses`:
/// Pleroma's `statuses_operation` — the five booleans, the one tag name and
/// the repeated visibility exclusion — every one a no-op in its [`Default`]
/// spelling, the cursors and `limit` belonging to [`PageParams`]. The
/// booleans are [`Flag`]s because that is what this repository models every
/// boolean with (see [`crate::models::user`]). `only_reblogs`, which
/// Pleroma's own spec lists beside these, is not a parameter of this
/// endpoint's ticket and is never read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StatusesParams {
    /// `pinned`: only the account's pinned statuses, in pin order, off the
    /// pagination keys.
    pinned: Flag,
    /// `only_media`: keep only statuses carrying an attachment.
    only_media: Flag,
    /// `exclude_replies`: drop rows that answer another status.
    exclude_replies: Flag,
    /// `exclude_reblogs`: drop the account's reblog rows.
    exclude_reblogs: Flag,
    /// `with_muted`: skip the viewer's block/mute exclusions (Pleroma's
    /// `with_muted`, default `false`).
    with_muted: Flag,
    /// `tagged`: keep only statuses carrying this tag, in the spelling the
    /// `hashtags` column stores; see [`tagged_name`] for the value a request
    /// that names nothing usable gets. `None` when the parameter is absent.
    tagged: Option<String>,
    /// `exclude_visibilities[]`: drop rows whose visibility is one of these;
    /// names that are not one of the five stored spellings are ignored.
    exclude_visibilities: Vec<Visibility>,
}

impl StatusesParams {
    /// Reads the parameters out of a raw query string (what `Uri::query`
    /// returns, percent-decoding and all), the same reading the timeline
    /// routes give: booleans through [`is_true`], `exclude_visibilities` in
    /// both the bracketed and the bare spelling through [`push_visibility`],
    /// `tagged` through [`tagged_name`]. The last occurrence of a key wins;
    /// an unrelated key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let mut found = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "pinned" => found.pinned = Flag::new(is_true(&value)),
                "only_media" => found.only_media = Flag::new(is_true(&value)),
                "exclude_replies" => found.exclude_replies = Flag::new(is_true(&value)),
                "exclude_reblogs" => found.exclude_reblogs = Flag::new(is_true(&value)),
                "with_muted" => found.with_muted = Flag::new(is_true(&value)),
                "tagged" => found.tagged = Some(tagged_name(&value)),
                "exclude_visibilities" | "exclude_visibilities[]" => {
                    push_visibility(&mut found.exclude_visibilities, &value);
                }
                _ => {}
            }
        }
        found
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::Account`] of `user_id` as `viewer` (`None` for an
    /// anonymous read), narrowed by these parameters and paged by `page` —
    /// except under `pinned`, which reads the whole pinned set on a page of
    /// its own (see [`every_pin`]).
    fn into_timeline(self, user_id: Uuid, viewer: Option<User>, page: PageParams) -> TimelineQuery {
        let pinned = self.pinned;
        TimelineQuery {
            viewer,
            source: TimelineSource::Account { user_id },
            filters: TimelineFilters {
                only_media: self.only_media,
                exclude_replies: self.exclude_replies,
                exclude_reblogs: self.exclude_reblogs,
                with_muted: self.with_muted,
                tagged: self.tagged,
                exclude_visibilities: self.exclude_visibilities,
                pinned_only: pinned,
                ..TimelineFilters::default()
            },
            page: if pinned.get() { every_pin() } else { page },
        }
    }
}

/// The `tagged` parameter in the spelling the `hashtags` column stores (see
/// [`normalize_tag`]). A value that leaves nothing usable — `tagged=`,
/// `tagged=%23` — becomes the empty name, which no stored tag can carry:
/// such a request is answered the empty page, not the unfiltered one.
fn tagged_name(raw: &str) -> String {
    normalize_tag(raw).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use crate::api::accounts::test_support::local_and_remote;

    use super::*;

    /// The parsed status-listing parameters, from a raw query string.
    fn statuses_parsed(query: &str) -> StatusesParams {
        StatusesParams::parse(Some(query))
    }

    /// A page cursor of `n`, the request's half of [`PageParams`] the
    /// listing hands its query.
    fn page_of(limit: i64) -> PageParams {
        PageParams {
            max_id: Some(Uuid::now_v7()),
            min_id: None,
            since_id: None,
            limit,
            offset: None,
        }
    }

    #[test]
    fn the_status_params_read_their_flags_and_ignore_the_rest() {
        let on = statuses_parsed(
            "pinned=true&only_media=1&exclude_replies=TRUE&exclude_reblogs=yes\
             &with_muted=true&limit=5&max_id=abc&since_id=def",
        );
        assert!(on.pinned.get(), "`pinned=true`");
        assert!(on.only_media.get(), "`only_media=1`");
        assert!(
            on.exclude_replies.get(),
            "`exclude_replies` is case-insensitive"
        );
        assert!(!on.exclude_reblogs.get(), "`yes` is not true here");
        assert!(on.with_muted.get(), "`with_muted=true`");
        assert_eq!(on.tagged, None, "no `tagged` names no tag");
        assert_eq!(
            on.exclude_visibilities,
            Vec::new(),
            "the page keys are `PageParams`' and were not looked at"
        );
        assert_eq!(
            statuses_parsed(""),
            StatusesParams::default(),
            "an empty query narrows nothing"
        );
        let off = statuses_parsed("pinned=false&only_media=&with_muted=0");
        assert!(
            !off.pinned.get() && !off.only_media.get() && !off.with_muted.get(),
            "`false`, empty and `0` are all off"
        );
        let last = statuses_parsed("pinned=true&pinned=false");
        assert!(!last.pinned.get(), "the last occurrence wins");
    }

    #[test]
    fn the_status_params_read_the_tag_and_the_visibility_exclusions() {
        assert_eq!(
            statuses_parsed("tagged=%23Rust").tagged.as_deref(),
            Some("rust"),
            "`#` is stripped and the name lowercased"
        );
        assert_eq!(
            statuses_parsed("tagged=").tagged.as_deref(),
            Some(""),
            "an empty `tagged` names no stored tag, so it matches nothing"
        );
        assert_eq!(
            statuses_parsed("tagged=%23").tagged.as_deref(),
            Some(""),
            "and so does a lone `#`"
        );
        let excluded = statuses_parsed(
            "exclude_visibilities[]=public&exclude_visibilities=Private&exclude_visibilities[]=direct",
        );
        assert_eq!(
            excluded.exclude_visibilities,
            vec![Visibility::Public, Visibility::Direct],
            "both spellings fill the one list and a capitalization names nothing"
        );
    }

    /// The query a plain request builds: the account's own timeline with the
    /// request's filters and the request's page.
    #[tokio::test]
    async fn the_status_query_is_the_accounts_own_timeline() {
        let (_db, alice, _bob) = local_and_remote().await;
        let alice_id = alice.id;
        let page = page_of(7);
        let query =
            statuses_parsed("only_media=true&exclude_reblogs=1&tagged=rust&with_muted=true")
                .into_timeline(alice_id, Some(alice), page);
        assert_eq!(
            query.source,
            TimelineSource::Account { user_id: alice_id },
            "the read is the account's own timeline"
        );
        assert_eq!(query.page, page, "the request's page is threaded through");
        assert_eq!(
            query.filters.only_media,
            Flag::new(true),
            "`only_media` rides from the query string"
        );
        assert_eq!(
            query.filters.exclude_reblogs,
            Flag::new(true),
            "so does `exclude_reblogs`, which is what takes the boosts out"
        );
        assert_eq!(
            query.filters.with_muted,
            Flag::new(true),
            "and the viewer's own exclusions can be lifted"
        );
        assert_eq!(
            query.filters.tagged.as_deref(),
            Some("rust"),
            "and the `tagged` name narrows to that tag"
        );
        assert!(
            !query.filters.pinned_only.get() && !query.filters.exclude_replies.get(),
            "what the request never asked for stays off"
        );
        assert!(
            query
                .viewer
                .as_ref()
                .is_some_and(|viewer| viewer.id == alice_id),
            "the query owns the authenticated viewer"
        );
    }

    /// `pinned` pages the whole featured set instead of the request's page:
    /// the pagination keys are ignored, and the pin filter is what the
    /// fetch runs on.
    #[tokio::test]
    async fn the_pinned_query_reads_every_pin_off_the_request_page() {
        let (_db, alice, _bob) = local_and_remote().await;
        let page = page_of(2);
        let query = statuses_parsed("pinned=1&only_media=true").into_timeline(alice.id, None, page);
        assert!(
            query.filters.pinned_only.get(),
            "`pinned` is the author's-pin filter"
        );
        assert_eq!(
            query.filters.only_media,
            Flag::new(true),
            "the other filters still apply"
        );
        assert!(query.viewer.is_none(), "an anonymous request has no viewer");
        assert_eq!(
            query.page,
            every_pin(),
            "the request's cursors and limit are ignored"
        );
        assert_eq!(
            (query.page.limit, query.page.max_id, query.page.offset),
            (i64::MAX, None, None),
            "that page is the whole set, uncursored"
        );
    }

    #[test]
    fn tagged_names_are_stored_spellings_or_nothing_at_all() {
        assert_eq!(
            tagged_name("#Rust"),
            "rust",
            "the written spelling normalizes"
        );
        assert_eq!(tagged_name(""), "", "an empty value names no tag");
        assert_eq!(
            tagged_name(" ## "),
            "",
            "and so does whitespace around a `#`"
        );
    }
}
