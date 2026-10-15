//! Tags over HTTP: showing one hashtag, following and unfollowing it, the
//! followed list, and the empty trends stubs (`docs/ARCHITECTURE.md`
//! decision 1).
//!
//! The reference is Pleroma's `TagController`
//! (`lib/pleroma/web/mastodon_api/controllers/tag_controller.ex`) and
//! `Pleroma.User.HashtagFollow` (`lib/pleroma/user/hashtag_follow.ex`).
//! Where the ticket pins a different behaviour it wins (decision 16):
//! Pleroma renders `history` as the empty list and 404s a `follow` of a tag
//! it never stored — here every tag render carries the last seven UTC days
//! of usage, computed from the statuses that use the tag, and `follow`
//! stores the tag row before following it.
//!
//! - `GET /api/v1/tags/{name}` takes no authentication (Pleroma's scopes
//!   plug for `show` lists `["read"]` and its plug proceeds unauthenticated:
//!   an anonymous request gets the Tag without a `following` key, a
//!   presented user token must grant `read`, and an invalid token is the
//!   401, never a silently anonymous show). An unknown tag is 404
//!   `{"error":"Hashtag not found"}` — Pleroma's own answer, not the empty
//!   tag Mastodon would build. The `{name}` segment is normalized like
//!   every written tag: `#Rust` shows `rust`.
//! - `POST /api/v1/tags/{name}/follow` needs `write:follows`. The tag row
//!   is created on demand when a valid name names none
//!   ([`hashtag::find_or_create`]); a name normalization rejects (blank, or
//!   over the stored-name bound) is the 422 `{"error":"Invalid hashtag"}`.
//!   The write ([`hashtag_follow::follow`]) is `ON CONFLICT DO NOTHING`, so
//!   following twice is one follow. The answer is the full Tag with
//!   `following: true`.
//! - `POST /api/v1/tags/{name}/unfollow` needs `write:follows` and creates
//!   nothing: an invalid name is the same 422, an unknown-but-valid tag the
//!   same 404 as `show`, and an unfollowed tag loses nothing — the write is
//!   idempotent and the answer is the Tag with `following: false`.
//! - `GET /api/v1/followed_tags` needs `read:follows`: the caller's follows
//!   as full Tags (`following: true` on each), newest follow first,
//!   [`OrdinalPageParams`] keyset pagination and a `Link` header whose
//!   cursors are [`hashtag_follow::Entry::position`] values — the order the
//!   tags were followed in, the one order the listing has (Pleroma pages it
//!   by the same integer ordinal, `id_type: :integer`).
//! - `GET /api/v1/trends`, `/api/v1/trends/tags`, `/api/v1/trends/statuses`
//!   and `/api/v1/trends/links` answer the empty array: Pleroma computes no
//!   trends, and the ticket keeps it that way. The routes exist so clients
//!   probing them get `[]` instead of a routing 404.
//!
//! Followed tags do *not* pull statuses into the Home timeline (Mastodon
//! does; Pleroma does not, and the ticket says do not).

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use chrono::{NaiveDate, NaiveTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::{AuthUser, MaybeAuthUser},
    entities::tag::{Tag, TagHistory},
    error::{AppError, AppResult},
    models::{hashtag, hashtag_follow},
    oauth::scopes::{READ, READ_FOLLOWS, WRITE_FOLLOWS},
    pagination::{MAX_LIMIT, OrdinalPageParams, link_header, set_link_header},
    state::AppState,
};

/// How many UTC days a tag's `history` covers, newest first: today and the
/// six before it (Mastodon's week of `TagHistory`).
const HISTORY_DAYS: u64 = 7;
/// 404 body for a name no `hashtags` row holds — Pleroma's
/// `render_error(:not_found, "Hashtag not found")`.
const TAG_NOT_FOUND: &str = "Hashtag not found";
/// 422 body for a path segment that normalizes to no usable tag name.
const INVALID_HASHTAG: &str = "Invalid hashtag";
/// The `Link` header's query parameters carry nothing but the cursor: the
/// followed-tags page has no filter to repeat on either link.
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1/tags`, `/api/v1/followed_tags` and `/api/v1/trends` router,
/// merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tags/{name}", get(show_tag))
        .route("/tags/{name}/follow", post(follow_tag))
        .route("/tags/{name}/unfollow", post(unfollow_tag))
        .route("/followed_tags", get(list_followed_tags))
        .route("/trends", get(empty_array))
        .route("/trends/tags", get(empty_array))
        .route("/trends/statuses", get(empty_array))
        .route("/trends/links", get(empty_array))
}

/// `GET /api/v1/tags/{name}` — the tag in its full form, `following` set
/// only when the request was made by a user.
///
/// Auth optional, as Pleroma's scopes plug runs `show` with `["read"]` and
/// no authentication requirement: an anonymous request proceeds and gets no
/// `following` key, a request that presents a user token must grant `read`
/// (Pleroma's `OAuthScopesPlug` wording), and an invalid token is the 401.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`MaybeAuthUser`] for a presented token
/// that does not resolve; [`AppError::Forbidden`] from
/// [`require_user_scope`](MaybeAuthUser::require_user_scope) for a user
/// token without `read`; [`AppError::NotFoundMessage`] with
/// [`TAG_NOT_FOUND`] for a name that is no stored tag. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn show_tag(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_user_scope(READ)?;
    let tag = known_tag(&state, &name).await?;
    let following = match &auth.0 {
        Some(user) => Some(hashtag_follow::is_following(&state.db, user.id, tag.id).await?),
        None => None,
    };
    Ok(Json(render_tag(&state, &tag, following).await?))
}

/// `POST /api/v1/tags/{name}/follow` — create the tag row when a valid
/// name names none, record the follow (idempotent), and answer the full
/// Tag with `following: true`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows` check; [`AppError::Unprocessable`] with
/// [`INVALID_HASHTAG`] for a name normalization rejects. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn follow_tag(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require(WRITE_FOLLOWS)?;
    let usable = usable_name(&name)?;
    let tag = hashtag::find_or_create(&state.db, &usable)
        .await?
        .ok_or_else(|| AppError::Unprocessable(INVALID_HASHTAG.to_owned()))?;
    let new = hashtag_follow::follow(&state.db, auth.0.id, tag.id).await?;
    debug!(
        hashtag = %tag.id,
        user = %auth.0.id,
        new,
        "hashtag followed"
    );
    Ok(Json(render_tag(&state, &tag, Some(true)).await?))
}

/// `POST /api/v1/tags/{name}/unfollow` — drop the follow (idempotent: an
/// unfollowed tag loses nothing) and answer the full Tag with
/// `following: false`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows` check; [`AppError::Unprocessable`] with
/// [`INVALID_HASHTAG`] for a name normalization rejects;
/// [`AppError::NotFoundMessage`] with [`TAG_NOT_FOUND`] for an
/// unknown-but-valid tag (nothing follows, and nothing is created). Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn unfollow_tag(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require(WRITE_FOLLOWS)?;
    let usable = usable_name(&name)?;
    let tag = known_tag(&state, &usable).await?;
    let removed = hashtag_follow::unfollow(&state.db, auth.0.id, tag.id).await?;
    debug!(
        hashtag = %tag.id,
        user = %auth.0.id,
        removed,
        "hashtag unfollowed"
    );
    Ok(Json(render_tag(&state, &tag, Some(false)).await?))
}

/// `GET /api/v1/followed_tags` — the caller's followed tags as full Tags,
/// newest follow first, one [`OrdinalPageParams`] page at a time (default
/// 20, at most [`MAX_LIMIT`], `limit=0` every follow) under a `Link` header
/// keyed on [`hashtag_follow::Entry::position`] — the order the tags were
/// followed in.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:follows` check; [`AppError::BadRequest`] from
/// [`OrdinalPageParams`] for a cursor that is not a number. Any database
/// failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
async fn list_followed_tags(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Tag>>)> {
    auth.require(READ_FOLLOWS)?;
    let page = OrdinalPageParams::from_query(uri.query(), MAX_LIMIT)?;
    let entries = hashtag_follow::page(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "followed tags listed"
    );
    let link = page_link(&state.config.base_url, uri.path(), &entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let mut tags = Vec::with_capacity(entries.len());
    for entry in &entries {
        tags.push(render_tag(&state, &entry.hashtag, Some(true)).await?);
    }
    Ok((headers, Json(tags)))
}

/// The trends endpoints' shared answer: `[]`, no authentication, no
/// parameters — Pleroma computes no trending tags, statuses or links, and
/// the ticket keeps all four routes empty.
async fn empty_array() -> AppResult<Json<Vec<Value>>> {
    Ok(Json(Vec::new()))
}

/// The stored row the `{name}` path segment names, or the 404 the client is
/// owed: [`hashtag::get_by_name`] does the same normalization the storage
/// key went through (`#Rust` finds `rust`), and an unusable name simply
/// holds no row.
///
/// # Errors
///
/// [`AppError::NotFoundMessage`] with [`TAG_NOT_FOUND`]; any database
/// failure arrives through `?` as [`AppError::Database`].
async fn known_tag(state: &AppState, name: &str) -> AppResult<hashtag::Hashtag> {
    hashtag::get_by_name(&state.db, name)
        .await?
        .ok_or_else(|| AppError::NotFoundMessage(TAG_NOT_FOUND.to_owned()))
}

/// The `{name}` path segment of a write, checked before anything is stored:
/// the normalized text, or the 422 [`INVALID_HASHTAG`] the endpoint owes a
/// name that trims, `#`-strips and lowercases to nothing (or to more than
/// the stored-name bound).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_HASHTAG`].
fn usable_name(name: &str) -> AppResult<String> {
    hashtag::normalize_name(name).ok_or_else(|| AppError::Unprocessable(INVALID_HASHTAG.to_owned()))
}

/// The full Tag entity for `tag` as `following` sees it: the two identity
/// keys, [`week`] of usage read for the tag's id, and the viewer's follow
/// state (`None` omits the key entirely).
///
/// # Errors
///
/// Any database failure behind [`hashtag::day_counts`] arrives as
/// [`AppError::Database`].
async fn render_tag(
    state: &AppState,
    tag: &hashtag::Hashtag,
    following: Option<bool>,
) -> AppResult<Tag> {
    let counts = week_counts(&state.db, tag.id).await?;
    Ok(Tag::with_history(
        &state.config.base_url,
        &tag.name,
        week(Utc::now().date_naive(), &counts),
        following,
    ))
}

/// The tag's live per-UTC-day counts inside the [`HISTORY_DAYS`] window that
/// ends today: [`hashtag::day_counts`] read from the window's first midnight.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn week_counts(pool: &PgPool, hashtag_id: Uuid) -> AppResult<Vec<(NaiveDate, i64, i64)>> {
    let today = Utc::now().date_naive();
    let first_midnight = today
        .checked_sub_days(chrono::Days::new(HISTORY_DAYS - 1))
        .unwrap_or(today)
        .and_time(NaiveTime::MIN)
        .and_utc();
    hashtag::day_counts(pool, hashtag_id, first_midnight).await
}

/// The `history` array itself: the [`HISTORY_DAYS`] UTC days ending at
/// `today`, newest first, each entry the day's `(uses, accounts)` from
/// `counts` or zeros — a day no status used the tag on still rides in the
/// array (Mastodon renders the whole week), and a count outside the window
/// is dropped.
fn week(today: NaiveDate, counts: &[(NaiveDate, i64, i64)]) -> Vec<TagHistory> {
    let mut by_day: HashMap<NaiveDate, (i64, i64)> = counts
        .iter()
        .map(|&(day, uses, accounts)| (day, (uses, accounts)))
        .collect();
    (0..HISTORY_DAYS)
        .filter_map(|ago| today.checked_sub_days(chrono::Days::new(ago)))
        .map(|day| {
            let (uses, accounts) = by_day.remove(&day).unwrap_or_default();
            TagHistory::new(
                day.and_time(NaiveTime::MIN).and_utc().timestamp(),
                uses,
                accounts,
            )
        })
        .collect()
}

/// The `Link` header value for a fetched followed-tags page: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`hashtag_follow::Entry::position`] — the cursors key on the follow
/// ordinal, not on the tags' ids — and `None` for an empty page.
fn page_link(base_url: &str, path: &str, entries: &[hashtag_follow::Entry]) -> Option<String> {
    let newest = entries.first()?.position.to_string();
    let oldest = entries.last()?.position.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    /// `year-month-day` as a [`NaiveDate`], for the week fixtures.
    fn date(ymd: &str) -> NaiveDate {
        NaiveDate::parse_from_str(ymd, "%Y-%m-%d").expect("the fixture date is valid")
    }

    /// The unix seconds of UTC midnight on `day`, as a history entry's `day`
    /// text is spelled.
    fn midnight(day: NaiveDate) -> String {
        day.and_time(NaiveTime::MIN)
            .and_utc()
            .timestamp()
            .to_string()
    }

    #[test]
    fn the_week_is_seven_days_newest_first_with_the_zerod_gaps() {
        let counts = [
            (date("2024-01-29"), 5, 2),
            (date("2024-01-31"), 1, 1),
            (date("2023-12-31"), 9, 9),
        ];
        let history = week(date("2024-01-31"), &counts);
        assert_eq!(history.len(), 7, "seven days, counted or empty");
        let rendered: Vec<(&str, &str, &str)> = history
            .iter()
            .map(|entry| {
                (
                    entry.day.as_str(),
                    entry.uses.as_str(),
                    entry.accounts.as_str(),
                )
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                (midnight(date("2024-01-31")).as_str(), "1", "1"),
                (midnight(date("2024-01-30")).as_str(), "0", "0"),
                (midnight(date("2024-01-29")).as_str(), "5", "2"),
                (midnight(date("2024-01-28")).as_str(), "0", "0"),
                (midnight(date("2024-01-27")).as_str(), "0", "0"),
                (midnight(date("2024-01-26")).as_str(), "0", "0"),
                (midnight(date("2024-01-25")).as_str(), "0", "0"),
            ],
            "newest first, every count a string, the unused days zero-filled, \
             and the out-of-window day dropped"
        );
    }

    #[test]
    fn an_empty_history_is_seven_zero_days() {
        let history = week(date("2024-02-05"), &[]);
        assert_eq!(
            history.len(),
            7,
            "a stored-but-unused tag still shows the week"
        );
        assert!(
            history
                .iter()
                .all(|entry| entry.uses == "0" && entry.accounts == "0"),
            "no counts yet"
        );
        assert_eq!(history[0].day, midnight(date("2024-02-05")));
        assert_eq!(history[6].day, midnight(date("2024-01-30")));
    }

    #[test]
    fn a_usable_name_is_the_stored_normalization() {
        use crate::util::entities::MAX_HASHTAG_LEN;

        assert_eq!(usable_name("#Foo").expect("a plain name"), "foo");
        assert_eq!(
            usable_name(" #Foo Bar ").expect("spaces are content"),
            "foo bar"
        );
        let long = "t".repeat(MAX_HASHTAG_LEN + 1);
        for rejected in ["", "   ", "#", "##", long.as_str()] {
            let err = usable_name(rejected).expect_err("the name is unusable");
            assert_eq!(
                err.to_string(),
                INVALID_HASHTAG,
                "with the ticket's wording"
            );
        }
    }
}
