//! Atom and RSS feeds over HTTP: the account timeline feeds and the tag
//! feeds (`docs/ARCHITECTURE.md` decision 3).
//!
//! This router is merged at the top level of [`crate::app`], not under
//! `/api`: the URLs it serves are the ones the actor route redirects HTML
//! clients' readers towards and the ones `WebFinger`-era syndication
//! discovers at `{BASE_URL}/users/{nickname}/feed.atom` and
//! `{BASE_URL}/tags/{tag}.rss`. The reference is Pleroma's `Feed`
//! controllers (`lib/pleroma/web/feed/user_controller.ex`,
//! `tag_controller.ex`, `feed_view.ex`); where the ticket pins a different
//! behaviour it wins (decision 16).
//!
//! - `GET /users/{nickname}/feed.atom`, `/feed.rss` and `/feed` serve the
//!   newest `FEED_LIMIT` live, `public`, root (neither reply nor reblog)
//!   statuses of that local account, newest first; `?max_id=` keys the next
//!   page on the status id (decision 4). `/feed` without an extension
//!   negotiates: an `Accept` naming `application/rss+xml` gets RSS,
//!   anything else gets Atom. An unknown or remote nickname is 404
//!   `{"error":"Not found"}`, a deactivated one 410 `{"error":"Gone"}` —
//!   the actor route's answers (decision 7).
//! - `GET /tags/{tag}.rss`, `/tags/{tag}.atom` and `/tags/{tag}` serve the
//!   newest public tagged statuses the same way. A trailing `.rss`/`.atom`
//!   is parsed out of the `{tag}` segment (matchit has no suffix
//!   parameters, and Pleroma's `parse_tag` does the same); an unknown tag
//!   is not an error: a valid, empty feed answers (200).
//! - Both areas honor Pleroma's `:restrict_unauthenticated`: an
//!   unauthenticated request to a feed of a restricted instance — the
//!   account feeds under `profiles.local`, the tag feeds under
//!   `activities.local` — is the 403 `{"error":"This API requires an
//!   authenticated user"}`; a request that presents a live user token
//!   passes (the read restriction is only about the anonymous reader).
//!   [`crate::services::instance_meta`] is the single source of the flags.
//!
//! The XML itself is built in [`crate::feeds`]; this module only assembles
//! the [`Feed`] descriptions from stored rows.

use std::collections::HashMap;

use axum::{
    Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, HeaderValue, Uri, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use chrono::Utc;
use serde_json::Value;
use sqlx::PgPool;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    auth::MaybeAuthUser,
    error::{AppError, AppResult},
    feeds::{self, Enclosure, Feed, FeedEntry},
    models::{
        hashtag,
        media_attachment::{self, MediaAttachment},
        status,
        user::{User, get_by_nickname},
    },
    services::instance_meta,
    state::AppState,
    util::id::parse_id,
};

/// How many statuses one feed page holds.
const FEED_LIMIT: i64 = 20;
/// How many characters of the status text an entry title carries.
const TITLE_CHARS: usize = 100;
/// 403 body when the instance restricts this feed area to readers
/// (Pleroma's `EnsureAuthenticatedPlug` wording, as in [`crate::auth`]).
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// 400 body for a `max_id` that is not id text (the wording of
/// [`crate::pagination`] for the same client mistake).
const INVALID_ID: &str = "Invalid id";
/// The `Accept` media type whose presence on a negotiated route means RSS.
const RSS_ACCEPT: &str = "application/rss+xml";
/// The `restrict_unauthenticated` group of profile reads.
const PROFILES_GROUP: &str = "profiles";
/// The `restrict_unauthenticated` group of activity reads.
const ACTIVITIES_GROUP: &str = "activities";
/// The local-instance half of both groups (the feeds only ever serve
/// local content).
const LOCAL_KEY: &str = "local";
/// The query parameter keying a feed's older pages.
const MAX_ID_PARAM: &str = "max_id";
/// The `Vary` value of the negotiated routes.
const VARY_ACCEPT: &str = "Accept";

/// The document format a feed route answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// An Atom document.
    Atom,
    /// An RSS 2.0 document.
    Rss,
}

impl Format {
    /// The `Content-Type` of this format's document, charset included.
    const fn content_type(self) -> &'static str {
        match self {
            Self::Atom => feeds::ATOM_CONTENT_TYPE,
            Self::Rss => feeds::RSS_CONTENT_TYPE,
        }
    }

    /// The extensionless routes' negotiation: an `Accept` entry naming
    /// `application/rss+xml` (ignoring any `;q=…` parameters, case as the
    /// media type does) asks for RSS, anything else — including no `Accept`
    /// at all — for Atom: the ticket's rule, and Pleroma's `SetFormatPlug`
    /// fallback of `"atom"`.
    fn negotiate(headers: &HeaderMap) -> Self {
        let wants_rss = accept_text(headers).is_some_and(|accept| {
            accept
                .split(',')
                .filter_map(|entry| entry.split(';').next())
                .any(|media_type| media_type.trim().eq_ignore_ascii_case(RSS_ACCEPT))
        });
        if wants_rss { Self::Rss } else { Self::Atom }
    }
}

/// The pieces of a feed request the serve functions share, once the
/// format has been decided.
#[derive(Debug)]
struct FeedRequest<'a> {
    /// The document format to render.
    format: Format,
    /// The request path, which becomes the feed's absolute self URL.
    path: &'a str,
    /// The raw query string, which may carry the `max_id` cursor.
    query: Option<&'a str>,
}

impl<'a> FeedRequest<'a> {
    /// The request for `format` behind `uri`.
    fn new(format: Format, uri: &'a Uri) -> Self {
        Self {
            format,
            path: uri.path(),
            query: uri.query(),
        }
    }
}

/// The readable `Accept` header value: `None` when the header is absent
/// or holds bytes that are not text (which is no preference, and the
/// Atom answer stands).
fn accept_text(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
}

/// The top-level feed router, merged by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users/{nickname}/feed", get(user_feed_negotiated))
        .route("/users/{nickname}/feed.atom", get(user_feed_atom))
        .route("/users/{nickname}/feed.rss", get(user_feed_rss))
        .route("/tags/{tag}", get(tag_feed))
}

/// `GET /users/{nickname}/feed` — the account feed in the format its
/// `Accept` header asks for (RSS when it names `application/rss+xml`,
/// Atom otherwise). `Vary: Accept` rides on every answer.
async fn user_feed_negotiated(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    headers: HeaderMap,
    Path(nickname): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<Response> {
    let request = FeedRequest::new(Format::negotiate(&headers), &uri);
    let mut response = serve_user_feed(&state, &auth, &nickname, &request).await?;
    with_vary_accept(&mut response);
    Ok(response)
}

/// `GET /users/{nickname}/feed.atom` — the account feed as Atom.
async fn user_feed_atom(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(nickname): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<Response> {
    let request = FeedRequest::new(Format::Atom, &uri);
    serve_user_feed(&state, &auth, &nickname, &request).await
}

/// `GET /users/{nickname}/feed.rss` — the account feed as RSS 2.0.
async fn user_feed_rss(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(nickname): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<Response> {
    let request = FeedRequest::new(Format::Rss, &uri);
    serve_user_feed(&state, &auth, &nickname, &request).await
}

/// `GET /tags/{tag}`, `/tags/{tag}.rss`, `/tags/{tag}.atom` — the tag
/// feed. A trailing `.rss`/`.atom` in the path segment fixes the format;
/// without one the `Accept` header negotiates, as on the account route.
/// An unknown tag answers a valid, empty feed, not a 404.
async fn tag_feed(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    headers: HeaderMap,
    Path(raw_tag): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<Response> {
    let (tag, explicit) = parse_tag_format(&raw_tag);
    let negotiated = explicit.is_none();
    let format = explicit.unwrap_or_else(|| Format::negotiate(&headers));
    let request = FeedRequest::new(format, &uri);
    let mut response = serve_tag_feed(&state, &auth, tag, &request).await?;
    if negotiated {
        with_vary_accept(&mut response);
    }
    Ok(response)
}

/// One feed answer with the `Vary` header its negotiation owes: the same
/// URL means Atom or RSS depending on the request header, so caches must
/// not mix the two.
fn with_vary_accept(response: &mut Response) {
    let previous = response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static(VARY_ACCEPT));
    drop(previous);
}

/// The account feed: the newest public root posts of the local, live
/// account named `nickname`.
///
/// # Errors
///
/// [`AppError::Forbidden`] when the instance restricts `profiles.local`
/// and `auth` names no user; [`AppError::BadRequest`] for an unusable
/// `max_id`; [`AppError::RouteNotFound`] for an unknown or remote
/// nickname; [`AppError::Gone`] for a deactivated one;
/// [`AppError::Database`] from the reads; [`AppError::Internal`] from the
/// builder ([`feeds::atom_document`]).
async fn serve_user_feed(
    state: &AppState,
    auth: &MaybeAuthUser,
    nickname: &str,
    request: &FeedRequest<'_>,
) -> AppResult<Response> {
    require_feed_access(
        auth.0.is_some(),
        &instance_meta::restrict_unauthenticated(),
        PROFILES_GROUP,
        LOCAL_KEY,
    )?;
    let cursor = feed_cursor(request.query)?;
    let user = local_active_user(state, nickname).await?;
    let posts = status::newest_public_posts(&state.db, user.id, cursor, FEED_LIMIT).await?;
    let entries = feed_entries(&state.db, &posts).await?;
    render(
        request.format,
        &user_feed_document(state, &user, request, entries),
    )
}

/// The tag feed: the newest public tagged posts, or the empty feed of a
/// tag this instance has never seen.
///
/// # Errors
///
/// As [`serve_user_feed`], except that an unknown tag is no error and no
/// account lookup happens.
async fn serve_tag_feed(
    state: &AppState,
    auth: &MaybeAuthUser,
    tag: &str,
    request: &FeedRequest<'_>,
) -> AppResult<Response> {
    require_feed_access(
        auth.0.is_some(),
        &instance_meta::restrict_unauthenticated(),
        ACTIVITIES_GROUP,
        LOCAL_KEY,
    )?;
    let cursor = feed_cursor(request.query)?;
    let entries = match hashtag::get_by_name(&state.db, tag).await? {
        Some(found) => {
            let posts = hashtag::public_posts(&state.db, found.id, cursor, FEED_LIMIT).await?;
            feed_entries(&state.db, &posts).await?
        }
        None => Vec::new(),
    };
    render(
        request.format,
        &tag_feed_document(state, tag, request, entries),
    )
}

/// The local, live account named `nickname`, or the error the feed route
/// owes: unknown or remote is [`AppError::RouteNotFound`], deactivated is
/// [`AppError::Gone`] — the actor route's answers for the same rows.
///
/// # Errors
///
/// As described; any database failure arrives through `?` as
/// [`AppError::Database`].
async fn local_active_user(state: &AppState, nickname: &str) -> AppResult<User> {
    let Some(user) = get_by_nickname(&state.db, nickname).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !user.local.get() {
        return Err(AppError::RouteNotFound);
    }
    if !user.is_active.get() {
        return Err(AppError::Gone);
    }
    Ok(user)
}

/// The `Feed` an account's feed route renders: the title the ticket spells
/// `{name}'s timeline` (the display name, or the nickname when the account
/// never set one), the profile's HTML URL as the alternate, the bio as the
/// `RSS` description, and the newest entry's timestamp (the account's own
/// while the feed is empty) as `updated`.
fn user_feed_document(
    state: &AppState,
    user: &User,
    request: &FeedRequest<'_>,
    entries: Vec<FeedEntry>,
) -> Feed {
    let name = display_name(user);
    let updated = entries
        .first()
        .map_or(user.updated_at, |entry| entry.updated);
    Feed {
        self_url: absolute(state.config.base_url.as_str(), request.path),
        alternate_url: format!("{}/{}", state.config.base_url, user.nickname),
        title: format!("{name}'s timeline"),
        description: user.bio.clone(),
        author_name: Some(name),
        updated,
        entries,
    }
}

/// The `Feed` a tag's feed route renders: the tag as title, no author, and
/// — like Pleroma's empty tag feed — the moment of the request as
/// `updated` when no status carries the tag yet.
fn tag_feed_document(
    state: &AppState,
    tag: &str,
    request: &FeedRequest<'_>,
    entries: Vec<FeedEntry>,
) -> Feed {
    let self_url = absolute(state.config.base_url.as_str(), request.path);
    let updated = entries.first().map_or_else(Utc::now, |entry| entry.updated);
    Feed {
        alternate_url: self_url.clone(),
        title: format!("#{tag}"),
        description: format!(
            "These are public statuses tagged with #{tag}. You can interact \
             with them if you have an account anywhere in the fediverse."
        ),
        author_name: None,
        self_url,
        updated,
        entries,
    }
}

/// `posts` as feed entries, with every row's attachments read in one
/// query: enclosure links in `position` order.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn feed_entries(pool: &PgPool, posts: &[status::Status]) -> AppResult<Vec<FeedEntry>> {
    let ids: Vec<Uuid> = posts.iter().map(|post| post.id).collect();
    let attachments = media_attachment::for_statuses(pool, &ids).await?;
    Ok(posts
        .iter()
        .map(|post| entry_for(post, &attachments))
        .collect())
}

/// One status as one entry: the `ActivityPub` uri as identity, the first
/// [`TITLE_CHARS`] characters of the source text as title, the sanitized
/// HTML as content, and the stored timestamps as `published`/`updated`.
fn entry_for(
    post: &status::Status,
    attachments: &HashMap<Uuid, Vec<MediaAttachment>>,
) -> FeedEntry {
    FeedEntry {
        id: post.uri.clone(),
        title: feeds::clip_title(&post.raw_text, TITLE_CHARS),
        content_html: post.content.clone(),
        url: post.url.clone(),
        published: post.inserted_at,
        updated: post.updated_at,
        enclosures: attachments
            .get(&post.id)
            .map_or_else(Vec::new, |attached| enclosures_of(attached)),
    }
}

/// A status's attachments as enclosure data; the size rides along when the
/// upload recorded one.
fn enclosures_of(attachments: &[MediaAttachment]) -> Vec<Enclosure> {
    attachments
        .iter()
        .map(|attachment| Enclosure {
            url: attachment.url.clone(),
            mime_type: attachment.content_type.clone(),
            size: attachment.file_size,
        })
        .collect()
}

/// The rendered feed as the response: the document body and the media
/// type of the format it was rendered in.
///
/// # Errors
///
/// [`AppError::Internal`] from the builder (see
/// [`feeds::atom_document`]).
fn render(format: Format, feed: &Feed) -> AppResult<Response> {
    let body = match format {
        Format::Atom => feeds::atom_document(feed)?,
        Format::Rss => feeds::rss_document(feed)?,
    };
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(format.content_type()),
        )],
        body,
    )
        .into_response())
}

/// The `max_id` cursor of a feed request's query string: `None` when the
/// parameter is absent (the first page), the parsed id when it is valid,
/// and the 400 [`INVALID_ID`] when it is present but is not id text —
/// the same convention [`crate::pagination`] keeps for the JSON
/// endpoints.
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`INVALID_ID`].
fn feed_cursor(query: Option<&str>) -> AppResult<Option<Uuid>> {
    let cursor = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .filter(|(key, _value)| key == MAX_ID_PARAM)
        .map(|(_key, value)| value.into_owned())
        .last();
    cursor.as_deref().map_or_else(
        || Ok(None),
        |text| {
            parse_id(text)
                .map(Some)
                .ok_or_else(|| AppError::BadRequest(INVALID_ID.to_owned()))
        },
    )
}

/// Whether `settings` restricts the anonymous reading of the `group`
/// area's local content. A malformed or absent flag is unrestricted:
/// access control fails open onto the public-feed default, and
/// [`instance_meta::restrict_unauthenticated`] is the single source of
/// truth for the flags.
fn area_restricted(settings: &Value, group: &str, key: &str) -> bool {
    settings
        .get(group)
        .and_then(|group| group.get(key))
        .and_then(Value::as_bool)
        .unwrap_or_default()
}

/// The `RESTRICT_UNAUTHENTICATED` gate of both feed areas: an anonymous
/// request to a restricted area is the 403 [`USER_REQUIRED`] (feeds are
/// anonymous by nature, so there is no half-way answer); a request that
/// authenticated as a user passes.
///
/// # Errors
///
/// [`AppError::Forbidden`] with [`USER_REQUIRED`] as described.
fn require_feed_access(
    authenticated: bool,
    settings: &Value,
    group: &str,
    key: &str,
) -> AppResult<()> {
    if authenticated || !area_restricted(settings, group, key) {
        return Ok(());
    }
    Err(AppError::Forbidden(USER_REQUIRED.to_owned()))
}

/// The account name the feed title uses: the display name, or the
/// nickname when the account never set one.
fn display_name(user: &User) -> String {
    if user.name.is_empty() {
        user.nickname.clone()
    } else {
        user.name.clone()
    }
}

/// The request path as an absolute URL on this instance.
fn absolute(base_url: &str, path: &str) -> String {
    format!("{base_url}{path}")
}

/// Pleroma's `parse_tag` (`lib/pleroma/web/feed/tag_controller.ex`): a
/// trailing `.rss` or `.atom` on the path segment selects the format and
/// is cut off the tag name; anything else names the tag whole and leaves
/// the format to negotiation. A leading-only dot (`.rss`) is not a format
/// suffix of an empty tag, it is the name.
fn parse_tag_format(raw: &str) -> (&str, Option<Format>) {
    match raw.rsplit_once('.') {
        Some((tag, "rss")) if !tag.is_empty() => (tag, Some(Format::Rss)),
        Some((tag, "atom")) if !tag.is_empty() => (tag, Some(Format::Atom)),
        _ => (raw, None),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    /// A [`HeaderMap`] holding only `Accept: <accept>`, for the negotiation
    /// tests.
    fn accept_headers(accept: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            let previous = headers.insert(
                header::ACCEPT,
                HeaderValue::from_str(accept).expect("the fixture value is header text"),
            );
            drop(previous);
        }
        headers
    }

    #[test]
    fn rss_accept_negotiates_rss_and_everything_else_atom() {
        for accept in [
            "application/rss+xml",
            "application/rss+xml; charset=utf-8",
            "text/html, application/rss+xml;q=0.9, */*;q=0.8",
            "APPLICATION/RSS+XML",
        ] {
            let headers = accept_headers(Some(accept));
            assert_eq!(
                Format::negotiate(&headers),
                Format::Rss,
                "{accept} names the RSS media type"
            );
        }
        for accept in [
            "application/atom+xml",
            "text/html",
            "*/*",
            "application/xml",
            "application/rss+xmlz",
        ] {
            let headers = accept_headers(Some(accept));
            assert_eq!(
                Format::negotiate(&headers),
                Format::Atom,
                "{accept} falls back to Atom"
            );
        }
        let absent = accept_headers(None);
        assert_eq!(
            Format::negotiate(&absent),
            Format::Atom,
            "a request that states nothing gets Atom"
        );
    }

    #[test]
    fn the_formats_answer_their_media_types() {
        assert_eq!(
            Format::Atom.content_type(),
            "application/atom+xml; charset=utf-8"
        );
        assert_eq!(
            Format::Rss.content_type(),
            "application/rss+xml; charset=utf-8"
        );
    }

    #[test]
    fn a_tag_segment_suffix_selects_the_format() {
        assert_eq!(
            parse_tag_format("rust"),
            ("rust", None),
            "no suffix is negotiation"
        );
        assert_eq!(
            parse_tag_format("rust.rss"),
            ("rust", Some(Format::Rss)),
            "the RSS suffix is cut off the name"
        );
        assert_eq!(
            parse_tag_format("rust.atom"),
            ("rust", Some(Format::Atom)),
            "the Atom suffix too"
        );
        assert_eq!(
            parse_tag_format("foo.bar.atom"),
            ("foo.bar", Some(Format::Atom)),
            "only the last dot splits, as Pleroma's parse_tag"
        );
        assert_eq!(
            parse_tag_format(".rss"),
            (".rss", None),
            "a lone suffix is a name, not a format"
        );
        assert_eq!(
            parse_tag_format("rust.RSS"),
            ("rust.RSS", None),
            "the suffix is matched case-sensitively, as Pleroma's"
        );
    }

    #[test]
    fn the_cursor_reads_max_id_when_present() {
        let id = Uuid::now_v7();
        assert_eq!(
            feed_cursor(None).expect("no query is no cursor"),
            None,
            "an absent query is a first page"
        );
        assert_eq!(
            feed_cursor(Some("")).expect("an empty query is a first page"),
            None
        );
        assert_eq!(
            feed_cursor(Some(&format!("max_id={id}"))).expect("a valid cursor parses"),
            Some(id),
            "the cursor comes back as the id"
        );
        assert_eq!(
            feed_cursor(Some(&format!("other=x&max_id={id}&limit=5")))
                .expect("position does not matter"),
            Some(id),
            "one parameter among others"
        );
        assert_eq!(
            feed_cursor(Some("max_id=first&max_id=second"))
                .expect_err("neither value is id text")
                .to_string(),
            INVALID_ID,
            "the last occurrence is checked, and rejected as no id"
        );
    }

    #[test]
    fn an_unusable_cursor_is_the_bad_request() {
        let err = feed_cursor(Some("max_id=not-a-uuid")).expect_err("not id text");
        assert_eq!(err.to_string(), INVALID_ID, "the wording of the API");
    }

    #[test]
    fn the_restriction_flags_are_read_by_group_and_key() {
        let settings = json!({
            "profiles": { "local": true, "remote": false },
            "activities": { "local": false, "remote": false },
            "timelines": { "local": true, "federated": false },
        });
        assert!(
            area_restricted(&settings, PROFILES_GROUP, LOCAL_KEY),
            "profiles.local is set"
        );
        assert!(
            !area_restricted(&settings, ACTIVITIES_GROUP, LOCAL_KEY),
            "activities.local is not"
        );
        assert!(
            !area_restricted(&settings, "no-such-group", LOCAL_KEY),
            "an unknown group restricts nothing"
        );
        assert!(
            !area_restricted(
                &json!({ "profiles": { "local": "yes" } }),
                PROFILES_GROUP,
                LOCAL_KEY
            ),
            "a flag that is not a boolean is no restriction"
        );
    }

    #[test]
    fn restricted_areas_403_only_anonymous_readers() {
        let restricted = json!({
            "profiles": { "local": true },
            "activities": { "local": true },
        });
        let err = require_feed_access(false, &restricted, PROFILES_GROUP, LOCAL_KEY)
            .expect_err("an anonymous reader is refused");
        assert_eq!(
            err.to_string(),
            USER_REQUIRED,
            "the refusal is Pleroma's authentication wording"
        );
        require_feed_access(true, &restricted, PROFILES_GROUP, LOCAL_KEY)
            .expect("a user token passes the gate");
        let tag_err = require_feed_access(false, &restricted, ACTIVITIES_GROUP, LOCAL_KEY)
            .expect_err("the tag area is gated the same way");
        assert_eq!(tag_err.to_string(), USER_REQUIRED, "with the same wording");
        require_feed_access(
            false,
            &instance_meta::restrict_unauthenticated(),
            PROFILES_GROUP,
            LOCAL_KEY,
        )
        .expect("a public instance feeds everybody");
    }

    #[test]
    fn self_urls_are_absolute_on_this_instance() {
        assert_eq!(
            absolute("https://test.example", "/users/alice/feed.atom"),
            "https://test.example/users/alice/feed.atom"
        );
    }
}
