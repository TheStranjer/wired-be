//! Pagination: the `max_id`/`min_id`/`since_id`/`limit`/`offset` query
//! parameters, the SQL they add to a list query, and the `Link` response
//! header.
//!
//! Two shapes of the same parameters live here. [`PageParams`] is the usual
//! one, keyset on a `UUIDv7` primary key. [`OrdinalPageParams`] is its
//! counterpart for a listing whose order is a `bigint` ordinal column rather
//! than an id (a list's members, keyed on `list_members.position`): its
//! cursors are those numbers and `limit=0` asks for every row.
//!
//! Per `docs/ARCHITECTURE.md` decision 4 every id is a `UUIDv7` whose text
//! sorts in creation order, so keyset pagination is plain comparison on the
//! id column — this mirrors Pleroma's `lib/pleroma/pagination.ex`
//! (default limit 20, cap `MAX_LIMIT`, `min_id` read ascending and flipped,
//! everything else descending). [`PageParams`] is the query-string extractor,
//! [`PageParams::apply_keyset`] appends the conditions to a
//! [`sqlx::QueryBuilder`], and [`link_header`], [`offset_link_header`] and
//! [`set_link_header`] produce the `Link` header Pleroma writes in
//! `add_link_headers` (`lib/pleroma/web/controller_helper.ex`).

use std::borrow::Cow;
use std::future::{self, Future};

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, HeaderValue, header::LINK, request::Parts},
};
use sqlx::{QueryBuilder, postgres::Postgres};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{error::AppError, util::id::parse_id};

/// Default page size when `limit` is absent, not a number, or not positive
/// (Pleroma's `@default_limit`).
const DEFAULT_LIMIT: i64 = 20;

/// Default cap for `limit` (Pleroma's `@max_limit`). Endpoints documented to
/// accept larger pages (up to 80) extract with [`PageParams::with_cap`]
/// instead of the plain [`PageParams`] extractor.
pub const MAX_LIMIT: i64 = 40;

/// 400 body for an id query parameter that is not a valid id.
const INVALID_ID: &str = "Invalid id";

/// The pagination query parameters of a request, parsed and defaulted.
///
/// Handlers take it as an extractor argument (`page: PageParams`) and feed
/// the cursor fields into [`PageParams::apply_keyset`]; offset-paginated
/// endpoints (admin lists, search) use [`PageParams::offset`] through
/// [`offset_link_header`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageParams {
    /// `max_id`: only rows strictly older (smaller id) than this one; the
    /// cursor a `rel="next"` link moves to.
    pub max_id: Option<Uuid>,
    /// `min_id`: only rows strictly newer (larger id) than this one, answered
    /// as the newest-first page that begins just after the cursor; read
    /// ascending and flipped by [`PageParams::finish`].
    pub min_id: Option<Uuid>,
    /// `since_id`: only rows strictly newer than this one, read descending
    /// like a first page; clients poll with it to get unseen rows.
    pub since_id: Option<Uuid>,
    /// `limit`: rows to fetch, already defaulted to 20 and capped at the
    /// extractor's limit (`MAX_LIMIT`, or the cap passed to
    /// [`PageParams::with_cap`]).
    pub limit: i64,
    /// `offset`: row offset for offset-paginated endpoints; `None` when
    /// absent, not a number, or negative.
    pub offset: Option<i64>,
}

impl PageParams {
    /// Parses the pagination parameters out of a raw query string (what
    /// `Uri::query()` returns, percent-decoding and all), defaulting `limit`
    /// to 20 and capping it at `cap`.
    ///
    /// A `limit` that is not a number or is not positive falls back to the
    /// default, one above `cap` falls to `cap` (Pleroma's `restrict(:limit)`).
    /// Duplicate keys: the last occurrence wins, as when a query-param map
    /// folds them. Unrelated keys are ignored.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] with `"Invalid id"` when `max_id`, `min_id`
    /// or `since_id` is present but is not a valid id.
    pub fn from_query(query: Option<&str>, cap: i64) -> Result<Self, AppError> {
        Self::from_query_with_default(query, DEFAULT_LIMIT, cap)
    }

    /// [`Self::from_query`] with an endpoint-specific default page size:
    /// Mastodon's account listings (the follow-request queue) answer
    /// `default_limit` rows when `limit` is absent or unusable, where the
    /// plain parse falls back to Pleroma's 20. `cap` still bounds an
    /// explicit `limit` the way [`Self::from_query`] caps it.
    ///
    /// # Errors
    ///
    /// As [`Self::from_query`].
    pub fn from_query_with_default(
        query: Option<&str>,
        default_limit: i64,
        cap: i64,
    ) -> Result<Self, AppError> {
        let mut raw = RawParams::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            raw.record(key.as_ref(), value);
        }
        Ok(Self {
            max_id: id_param(raw.max_id.as_deref())?,
            min_id: id_param(raw.min_id.as_deref())?,
            since_id: id_param(raw.since_id.as_deref())?,
            limit: clamp_limit(raw.limit.as_deref(), default_limit, cap),
            offset: parse_offset(raw.offset.as_deref()),
        })
    }

    /// [`Self::from_query`] on the request's query string with a custom
    /// `limit` cap: an endpoint documented to accept pages up to 80 extracts
    /// `PageParams::with_cap(80, &parts)?` instead of taking [`Self`] as an
    /// extractor argument.
    ///
    /// # Errors
    ///
    /// As [`Self::from_query`].
    pub fn with_cap(cap: i64, parts: &Parts) -> Result<Self, AppError> {
        Self::from_query(parts.uri.query(), cap)
    }

    /// Appends these parameters' keyset conditions, id ordering and `LIMIT`
    /// to `qb`, continuing the statement it already holds (a `SELECT ...
    /// WHERE ...`): each present cursor narrows `id_column` with a bind
    /// argument, checked in the order `max_id`, `min_id`, `since_id`.
    ///
    /// A `min_id` page is read ascending, straight off the index starting
    /// just after the cursor, so it can stop after one page; every other
    /// page is read newest-first. Pass the fetched rows through
    /// [`Self::finish`] so a response is always newest-first.
    pub fn apply_keyset(&self, qb: &mut QueryBuilder<Postgres>, id_column: &str) {
        // sqlx's builder methods hand back `&mut Self` for chaining; dropping
        // that on the floor would be an unused result (decision 14), so each
        // append binds it to an `_`-named value and lets the borrow end there.
        if let Some(max_id) = self.max_id {
            let _with_max: &mut _ = qb
                .push(" AND ")
                .push(id_column)
                .push(" < ")
                .push_bind(max_id);
        }
        if let Some(min_id) = self.min_id {
            let _with_min: &mut _ = qb
                .push(" AND ")
                .push(id_column)
                .push(" > ")
                .push_bind(min_id);
        }
        if let Some(since_id) = self.since_id {
            let _with_since: &mut _ = qb
                .push(" AND ")
                .push(id_column)
                .push(" > ")
                .push_bind(since_id);
        }
        let _ordered: &mut _ =
            qb.push(" ORDER BY ")
                .push(id_column)
                .push(if self.min_id.is_some() {
                    " ASC"
                } else {
                    " DESC"
                });
        let _limited: &mut _ = qb.push(" LIMIT ").push_bind(self.limit);
    }

    /// Brings rows fetched with a query built by [`Self::apply_keyset`] into
    /// the newest-first order every list is answered in: a `min_id` page is
    /// read ascending and comes back reversed, every other page is returned
    /// unchanged.
    #[must_use]
    pub fn finish<T>(&self, mut rows: Vec<T>) -> Vec<T> {
        if self.min_id.is_some() {
            rows.reverse();
        }
        rows
    }
}

/// The default-cap extractor: a handler argument `page: PageParams` parses
/// the query string with the [`MAX_LIMIT`] cap; a malformed id rejects as an
/// [`AppError`] (decision 8). Endpoints with a higher cap use
/// [`PageParams::with_cap`] inside the handler (an extractor cannot take the
/// cap as an argument).
impl<S> FromRequestParts<S> for PageParams
where
    S: Send + Sync,
{
    type Rejection = AppError;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        future::ready(Self::with_cap(MAX_LIMIT, parts))
    }
}

/// The pagination parameters of a listing whose keyset column is a `bigint`
/// ordinal rather than a [`UUIDv7`](uuid::Uuid) primary key.
///
/// One listing needs it: [`crate::models::list::members_page`] pages over the
/// order members were added in — `list_members.position`, the sequence its
/// migration adds — so its cursors and its [`Link`](link_header) header carry
/// position numbers instead of ids, and a client that wants the whole list at
/// once sends `limit=0`. Everything else is [`PageParams`]'s rule: default
/// page 20, capped at the caller's `cap`, `min_id` read ascending and flipped,
/// every other page newest-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrdinalPageParams {
    /// `max_id`: only rows with a strictly smaller position; the cursor a
    /// `rel="next"` link moves to.
    pub max_id: Option<i64>,
    /// `min_id`: only rows with a strictly larger position, read ascending
    /// and flipped by [`Self::finish`] so the page is still newest-first.
    pub min_id: Option<i64>,
    /// `since_id`: only rows with a strictly larger position, read descending
    /// like a first page.
    pub since_id: Option<i64>,
    /// `limit`: rows to fetch, defaulted to 20 and capped; `None` when the
    /// request asked for every row with `limit=0`.
    pub limit: Option<i64>,
}

impl OrdinalPageParams {
    /// Parses the ordinal pagination parameters out of a raw query string the
    /// way [`PageParams::from_query`] does, with two differences: a cursor has
    /// to be a number rather than an id, and `limit=0` means "every row"
    /// ([`Self::limit`] is `None`) instead of falling back to the default page.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] with `"Invalid id"` when `max_id`, `min_id` or
    /// `since_id` is present but is not a number.
    pub fn from_query(query: Option<&str>, cap: i64) -> Result<Self, AppError> {
        let mut raw = RawParams::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            raw.record(key.as_ref(), value);
        }
        Ok(Self {
            max_id: ordinal_param(raw.max_id.as_deref())?,
            min_id: ordinal_param(raw.min_id.as_deref())?,
            since_id: ordinal_param(raw.since_id.as_deref())?,
            limit: limit_or_all(raw.limit.as_deref(), cap),
        })
    }

    /// Appends these parameters' keyset conditions, position ordering and
    /// `LIMIT` to `qb`, continuing the statement it already holds, exactly as
    /// [`PageParams::apply_keyset`] does for an id column — except that no
    /// `LIMIT` is appended when [`Self::limit`] is `None`, the `limit=0` page
    /// that answers every row.
    pub fn apply_keyset(&self, qb: &mut QueryBuilder<Postgres>, column: &str) {
        if let Some(max_id) = self.max_id {
            let _with_max: &mut _ = qb.push(" AND ").push(column).push(" < ").push_bind(max_id);
        }
        if let Some(min_id) = self.min_id {
            let _with_min: &mut _ = qb.push(" AND ").push(column).push(" > ").push_bind(min_id);
        }
        if let Some(since_id) = self.since_id {
            let _with_since: &mut _ = qb
                .push(" AND ")
                .push(column)
                .push(" > ")
                .push_bind(since_id);
        }
        let _ordered: &mut _ = qb
            .push(" ORDER BY ")
            .push(column)
            .push(if self.min_id.is_some() {
                " ASC"
            } else {
                " DESC"
            });
        if let Some(limit) = self.limit {
            let _limited: &mut _ = qb.push(" LIMIT ").push_bind(limit);
        }
    }

    /// [`PageParams::finish`] for an ordinal page: a `min_id` fetch came back
    /// ascending and goes out reversed, every other page unchanged.
    #[must_use]
    pub fn finish<T>(&self, mut rows: Vec<T>) -> Vec<T> {
        if self.min_id.is_some() {
            rows.reverse();
        }
        rows
    }
}

/// One ordinal cursor: `None` when absent, the number when it parses, a 400
/// `"Invalid id"` when it is present but is not a number.
fn ordinal_param(text: Option<&str>) -> Result<Option<i64>, AppError> {
    let Some(text) = text else {
        return Ok(None);
    };
    text.parse::<i64>()
        .ok()
        .map(Some)
        .ok_or_else(|| AppError::BadRequest(INVALID_ID.to_owned()))
}

/// `limit` of an ordinal page: `0` — and only `0` — asks for every row, which
/// is no `LIMIT` at all; every other spelling follows [`clamp_limit`].
fn limit_or_all(text: Option<&str>, cap: i64) -> Option<i64> {
    if text.and_then(|text| text.parse::<i64>().ok()) == Some(0) {
        return None;
    }
    Some(clamp_limit(text, DEFAULT_LIMIT, cap))
}

/// The pagination keys found in a query string; anything else is never
/// looked at. A present-but-empty value stays present (an empty id is
/// invalid, not absent).
#[derive(Debug, Default)]
struct RawParams<'a> {
    /// Raw text of `max_id`.
    max_id: Option<Cow<'a, str>>,
    /// Raw text of `min_id`.
    min_id: Option<Cow<'a, str>>,
    /// Raw text of `since_id`.
    since_id: Option<Cow<'a, str>>,
    /// Raw text of `limit`.
    limit: Option<Cow<'a, str>>,
    /// Raw text of `offset`.
    offset: Option<Cow<'a, str>>,
}

impl<'a> RawParams<'a> {
    /// Remembers the value of a pagination key, the last occurrence of it
    /// winning; ignores every other key.
    fn record(&mut self, key: &str, value: Cow<'a, str>) {
        match key {
            "max_id" => self.max_id = Some(value),
            "min_id" => self.min_id = Some(value),
            "since_id" => self.since_id = Some(value),
            "limit" => self.limit = Some(value),
            "offset" => self.offset = Some(value),
            _ => {}
        }
    }
}

/// One id parameter: `None` when absent, the id when it parses, a 400
/// `"Invalid id"` when it is present but not a valid id.
fn id_param(text: Option<&str>) -> Result<Option<Uuid>, AppError> {
    let Some(text) = text else {
        return Ok(None);
    };
    parse_id(text)
        .map(Some)
        .ok_or_else(|| AppError::BadRequest(INVALID_ID.to_owned()))
}

/// `limit`: absent, non-numeric or non-positive text falls back to the
/// endpoint's `default` (Pleroma's 20, Mastodon's account listings 40),
/// larger values fall to the endpoint's `cap` (Pleroma's `restrict(:limit)`).
fn clamp_limit(text: Option<&str>, default: i64, cap: i64) -> i64 {
    let parsed = text
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(default);
    if parsed <= 0 {
        default
    } else {
        parsed.min(cap)
    }
}

/// `offset`: `None` when absent, non-numeric or negative; offset pagination
/// has no pages before the first row.
fn parse_offset(text: Option<&str>) -> Option<i64> {
    text.and_then(|text| text.parse::<i64>().ok())
        .filter(|&offset| offset >= 0)
}

/// Builds the `Link` header value for a keyset-paginated response.
///
/// This is Pleroma's `add_link_headers`: `newest_id` and `oldest_id` (the
/// id-column text of the first and last row of the newest-first page) become
/// the cursors, repeated with `extra_query` on both links:
///
/// ```text
/// <{base_url}{path}?…&max_id={oldest_id}>; rel="next", <…&min_id={newest_id}>; rel="prev"
/// ```
///
/// Returns `None` for an empty page — signalled by an empty id — which
/// Pleroma answers by sending no `Link` header at all.
#[must_use]
pub fn link_header(
    base_url: &str,
    path: &str,
    extra_query: &[(String, String)],
    newest_id: &str,
    oldest_id: &str,
) -> Option<String> {
    if newest_id.is_empty() || oldest_id.is_empty() {
        return None;
    }
    let next = page_url(base_url, path, extra_query, "max_id", oldest_id);
    let prev = page_url(base_url, path, extra_query, "min_id", newest_id);
    Some(format!(r#"<{next}>; rel="next", <{prev}>; rel="prev""#))
}

/// Builds the `Link` header value for an offset-paginated response.
///
/// Admin lists and search number their pages by `offset=` instead of by id
/// cursors. `params` carries the request's `offset` (`None` meaning the
/// first page) and `limit`; `count` is the number of rows the page returned:
///
/// - `rel="next"` when the page came back full, at one page further;
/// - `rel="prev"` when the request was not already on the first page, at one
///   page back (never below 0);
/// - `None` for an empty page, and for a first page that did not fill up —
///   there is nothing left to link to.
#[must_use]
pub fn offset_link_header(
    base_url: &str,
    path: &str,
    extra_query: &[(String, String)],
    params: &PageParams,
    count: usize,
) -> Option<String> {
    let rows = i64::try_from(count).ok()?;
    if rows == 0 {
        return None;
    }
    let offset = params.offset.unwrap_or(0);
    let mut links = Vec::new();
    if rows >= params.limit {
        let next = offset.saturating_add(rows).to_string();
        let url = page_url(base_url, path, extra_query, "offset", &next);
        links.push(format!(r#"<{url}>; rel="next""#));
    }
    if offset > 0 {
        let prev = offset.saturating_sub(params.limit).max(0).to_string();
        let url = page_url(base_url, path, extra_query, "offset", &prev);
        links.push(format!(r#"<{url}>; rel="prev""#));
    }
    if links.is_empty() {
        return None;
    }
    Some(links.join(", "))
}

/// Writes a `Link` header value (from [`link_header`] or
/// [`offset_link_header`]) into a response's headers; `None` — an empty page
/// — leaves them untouched, sending no `Link` header.
///
/// # Errors
///
/// [`AppError::Internal`] when the value holds bytes not allowed in an HTTP
/// header (only reachable through hand-rolled `base_url`/`extra_query`
/// text).
pub fn set_link_header(headers: &mut HeaderMap, link: Option<&str>) -> Result<(), AppError> {
    let Some(link) = link else {
        return Ok(());
    };
    let value = HeaderValue::from_str(link).map_err(|err| AppError::Internal(err.to_string()))?;
    let _replaced: Option<HeaderValue> = headers.insert(LINK, value);
    Ok(())
}

/// One `Link` target URL: `extra_query` and the cursor pair, all
/// form-urlencoded (a space becomes `+`, reserved bytes become `%XX`), hung
/// on `{base_url}{path}?`.
fn page_url(
    base_url: &str,
    path: &str,
    extra_query: &[(String, String)],
    key: &str,
    value: &str,
) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    // The serializer's `append_pair` returns `&mut Self` for chaining; bind
    // it so the statement has no unused result (decision 14).
    for (extra_key, extra_value) in extra_query {
        let _ser: &mut _ = serializer.append_pair(extra_key, extra_value);
    }
    let _ser: &mut _ = serializer.append_pair(key, value);
    format!("{base_url}{path}?{}", serializer.finish())
}

#[cfg(test)]
mod tests {
    use axum::{
        http::{Request, StatusCode, Uri},
        response::IntoResponse as _,
    };
    use sqlx::SqlStr;

    use super::*;

    /// A valid `max_id` cursor.
    const MAX_ID: &str = "01890b9a-2c1d-7f3a-9e4b-7c2d5a8b1e3f";
    /// A valid `min_id` cursor, a newer id than `MAX_ID`.
    const MIN_ID: &str = "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d";
    /// A valid `since_id` cursor, a newer id than `MIN_ID`.
    const SINCE_ID: &str = "01890b9a-2c1f-7d7e-9a0b-3c4d5e6f7a8b";

    /// Parses `query` with the default cap, expecting success.
    fn parse_query(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).unwrap()
    }

    /// Request parts carrying `uri`, the way the extractor receives them.
    fn parts_for(uri: &str) -> Parts {
        Request::get(uri.parse::<Uri>().unwrap())
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    /// The SQL these parameters append to a dummy select on `column`.
    fn keyset_sql(query: &str, column: &str) -> SqlStr {
        let params = parse_query(query);
        let mut qb = QueryBuilder::new("SELECT id FROM statuses WHERE 1=1");
        params.apply_keyset(&mut qb, column);
        qb.into_sql()
    }

    #[test]
    fn a_missing_or_empty_query_gets_the_defaults() {
        for query in [None, Some(""), Some("foo=bar&baz=1")] {
            let params = PageParams::from_query(query, MAX_LIMIT).unwrap();
            assert_eq!(
                params,
                PageParams {
                    max_id: None,
                    min_id: None,
                    since_id: None,
                    limit: 20,
                    offset: None,
                },
                "defaults for query {query:?}"
            );
        }
    }

    #[test]
    fn limit_is_parsed_capped_and_defaulted() {
        for (query, want) in [
            ("limit=5", 5),
            ("limit=40", 40),
            ("limit=41", 40),
            ("limit=100", 40),
            ("limit=0", 20),
            ("limit=-3", 20),
            ("limit=abc", 20),
            ("limit=3.5", 20),
            ("limit=99999999999999999999999999", 20),
        ] {
            assert_eq!(parse_query(query).limit, want, "limit from {query}");
        }
    }

    #[test]
    fn a_custom_default_sizes_absent_and_unusable_pages() {
        // The follow-request queue's pair: default 40, cap 80.
        for query in [
            None,
            Some(""),
            Some("limit=abc"),
            Some("limit=0"),
            Some("limit=-5"),
        ] {
            let params = PageParams::from_query_with_default(query, 40, 80).unwrap();
            assert_eq!(params.limit, 40, "default of 40 for query {query:?}");
        }
        for (query, want) in [("limit=5", 5), ("limit=60", 60), ("limit=100", 80)] {
            let params = PageParams::from_query_with_default(Some(query), 40, 80).unwrap();
            assert_eq!(params.limit, want, "explicit {query} under cap 80");
        }
        // The plain parse keeps Pleroma's 20 as its default.
        assert_eq!(
            PageParams::from_query(Some("limit=0"), 80).unwrap().limit,
            20,
            "from_query is from_query_with_default at the global default"
        );
    }

    #[test]
    fn a_custom_cap_lets_bigger_pages_through() {
        for (query, cap, want) in [
            ("limit=60", 80, 60),
            ("limit=100", 80, 80),
            ("limit=41", 80, 41),
            ("limit=5", 80, 5),
            ("limit=abc", 80, 20),
        ] {
            let got = PageParams::from_query(Some(query), cap).unwrap();
            assert_eq!(got.limit, want, "limit {query} under cap {cap}");
        }
    }

    #[test]
    fn id_params_are_parsed() {
        let params = parse_query(&format!(
            "max_id={MAX_ID}&min_id={MIN_ID}&since_id={SINCE_ID}"
        ));
        assert_eq!(params.max_id, Some(parse_id(MAX_ID).unwrap()), "max_id");
        assert_eq!(params.min_id, Some(parse_id(MIN_ID).unwrap()), "min_id");
        assert_eq!(
            params.since_id,
            Some(parse_id(SINCE_ID).unwrap()),
            "since_id"
        );
        let short = PageParams::from_query(
            Some(&format!("max_id={}", MAX_ID.replace('-', ""))),
            MAX_LIMIT,
        )
        .unwrap();
        assert_eq!(
            short.max_id,
            Some(parse_id(MAX_ID).unwrap()),
            "the hyphenless spelling is an id too"
        );
    }

    #[test]
    fn an_id_param_that_is_not_an_id_is_a_bad_request() {
        for value in ["nope", "not-an-id", "", SINCE_ID.replace('0', "g").as_str()] {
            for key in ["max_id", "min_id", "since_id"] {
                let query = format!("{key}={value}");
                let err = PageParams::from_query(Some(&query), MAX_LIMIT)
                    .expect_err(&format!("{query} must be rejected"));
                assert!(
                    matches!(err, AppError::BadRequest(_)),
                    "{query} is a 400, not {err}"
                );
                assert_eq!(err.to_string(), "Invalid id", "message for {query}");
                let response = err.into_response();
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "status for {query}"
                );
            }
        }
    }

    #[test]
    fn offset_must_be_a_number_and_cannot_be_negative() {
        assert_eq!(parse_query("offset=40").offset, Some(40), "a real offset");
        for query in ["offset=x", "offset=-1", "offset=1.5"] {
            assert_eq!(
                parse_query(query).offset,
                None,
                "{query} is no usable offset"
            );
        }
    }

    #[test]
    fn duplicate_keys_keep_the_last_value() {
        let params = parse_query(&format!("limit=5&limit=7&max_id={MIN_ID}&max_id={MAX_ID}"));
        assert_eq!(params.limit, 7, "last limit wins");
        assert_eq!(
            params.max_id,
            Some(parse_id(MAX_ID).unwrap()),
            "last max_id"
        );
    }

    #[test]
    fn percent_encoded_values_are_decoded_before_parsing() {
        let params = parse_query("max_id=01890b9a%2D2c1d%2D7f3a%2D9e4b%2D7c2d5a8b1e3f");
        assert_eq!(
            params.max_id,
            Some(parse_id(MAX_ID).unwrap()),
            "%2D is a hyphen"
        );
    }

    #[tokio::test]
    async fn the_extractor_parses_the_request_uri_with_the_default_cap() {
        let mut parts = parts_for(&format!(
            "/api/v1/timelines/public?limit=100&max_id={MAX_ID}"
        ));
        let params = PageParams::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(params.limit, 40, "the request cap is MAX_LIMIT");
        assert_eq!(params.max_id, Some(parse_id(MAX_ID).unwrap()), "max_id");

        let capped_parts = parts_for(&format!(
            "/api/v1/timelines/public?limit=100&min_id={MIN_ID}"
        ));
        let capped = PageParams::with_cap(80, &capped_parts).unwrap();
        assert_eq!(capped.limit, 80, "with_cap raises the cap");
        assert_eq!(capped.min_id, Some(parse_id(MIN_ID).unwrap()), "min_id");
    }

    #[tokio::test]
    async fn the_extractor_rejects_a_malformed_id() {
        let mut parts = parts_for("/api/v1/timelines/home?max_id=nope");
        let err = PageParams::from_request_parts(&mut parts, &())
            .await
            .expect_err("a bogus id must reject");
        assert_eq!(err.to_string(), "Invalid id", "the rejection message");
    }

    #[test]
    fn no_cursor_orders_descending_and_limits() {
        assert_eq!(
            keyset_sql("", "id"),
            "SELECT id FROM statuses WHERE 1=1 ORDER BY id DESC LIMIT $1",
            "first page"
        );
        assert_eq!(
            keyset_sql(&format!("max_id={MAX_ID}"), "id"),
            "SELECT id FROM statuses WHERE 1=1 AND id < $1 ORDER BY id DESC LIMIT $2",
            "max_id reads older rows, newest first"
        );
        assert_eq!(
            keyset_sql(&format!("since_id={SINCE_ID}"), "id"),
            "SELECT id FROM statuses WHERE 1=1 AND id > $1 ORDER BY id DESC LIMIT $2",
            "since_id stays descending"
        );
    }

    #[test]
    fn min_id_orders_ascending() {
        assert_eq!(
            keyset_sql(&format!("min_id={MIN_ID}"), "id"),
            "SELECT id FROM statuses WHERE 1=1 AND id > $1 ORDER BY id ASC LIMIT $2",
            "the page just after the cursor, read forward"
        );
        assert_eq!(
            keyset_sql(&format!("min_id={MIN_ID}&max_id={MAX_ID}"), "id"),
            "SELECT id FROM statuses WHERE 1=1 AND id < $1 AND id > $2 ORDER BY id ASC LIMIT $3",
            "min_id decides the order, and comes after max_id"
        );
        assert_eq!(
            keyset_sql(
                &format!("max_id={MAX_ID}&min_id={MIN_ID}&since_id={SINCE_ID}"),
                "id"
            ),
            "SELECT id FROM statuses WHERE 1=1 AND id < $1 AND id > $2 AND id > $3 \
             ORDER BY id ASC LIMIT $4",
            "conditions in max/min/since order"
        );
    }

    #[test]
    fn keyset_uses_the_given_id_column() {
        assert_eq!(
            keyset_sql(&format!("since_id={SINCE_ID}"), "s.id"),
            "SELECT id FROM statuses WHERE 1=1 AND s.id > $1 ORDER BY s.id DESC LIMIT $2",
            "the alias qualifies both condition and order"
        );
    }

    #[test]
    fn finish_flips_only_min_id_pages() {
        assert_eq!(
            parse_query(&format!("min_id={MIN_ID}")).finish(vec![1, 2, 3]),
            vec![3, 2, 1],
            "an ascending fetch is answered newest-first"
        );
        assert_eq!(
            parse_query(&format!("max_id={MAX_ID}")).finish(vec![1, 2, 3]),
            vec![1, 2, 3],
            "a descending fetch stays as it is"
        );
        assert_eq!(
            parse_query("").finish(vec![1, 2, 3]),
            vec![1, 2, 3],
            "a first page stays as it is"
        );
        assert_eq!(
            parse_query(&format!("min_id={MIN_ID}")).finish(Vec::<i64>::new()),
            Vec::<i64>::new(),
            "nothing to flip on an empty page"
        );
    }

    #[test]
    fn link_header_names_next_and_prev() {
        let link = link_header(
            "https://example.com",
            "/api/v1/timelines/public",
            &[],
            MIN_ID,
            MAX_ID,
        )
        .unwrap();
        assert_eq!(
            link,
            format!(
                "<https://example.com/api/v1/timelines/public?max_id={MAX_ID}>; rel=\"next\", \
                 <https://example.com/api/v1/timelines/public?min_id={MIN_ID}>; rel=\"prev\""
            ),
            "next walks to the oldest row, prev back to the newest"
        );
    }

    #[test]
    fn link_header_repeats_and_encodes_extra_query() {
        let extra = vec![
            ("tag".to_owned(), "fedi verse".to_owned()),
            ("q".to_owned(), "a&b=c".to_owned()),
        ];
        let link = link_header("https://h", "/api/v1/timelines/tag/x", &extra, "n1", "o1").unwrap();
        assert_eq!(
            link,
            "<https://h/api/v1/timelines/tag/x?tag=fedi+verse&q=a%26b%3Dc&max_id=o1>; \
             rel=\"next\", <https://h/api/v1/timelines/tag/x?tag=fedi+verse&q=a%26b%3Dc&min_id=n1>; \
             rel=\"prev\"",
            "extras come before the cursor, form-encoded"
        );
    }

    #[test]
    fn an_empty_keyset_page_gets_no_link_header() {
        for (newest, oldest) in [("", ""), ("n1", ""), ("", "o1")] {
            assert_eq!(
                link_header("https://h", "/p", &[], newest, oldest),
                None,
                "empty ids (newest={newest:?}, oldest={oldest:?}) mean no page"
            );
        }
    }

    #[test]
    fn offset_link_header_walks_by_pages() {
        let base = ("https://h", "/api/pleroma/admin/users", &[][..]);
        let first =
            offset_link_header(base.0, base.1, base.2, &parse_query("limit=20"), 20).unwrap();
        assert_eq!(
            first, "<https://h/api/pleroma/admin/users?offset=20>; rel=\"next\"",
            "a full first page may have a next, it has no prev"
        );

        let middle = offset_link_header(
            base.0,
            base.1,
            base.2,
            &parse_query("limit=20&offset=40"),
            20,
        )
        .unwrap();
        assert_eq!(
            middle,
            "<https://h/api/pleroma/admin/users?offset=60>; rel=\"next\", \
             <https://h/api/pleroma/admin/users?offset=20>; rel=\"prev\"",
            "a full middle page links both ways"
        );

        let tail = offset_link_header(
            base.0,
            base.1,
            base.2,
            &parse_query("limit=20&offset=40"),
            5,
        )
        .unwrap();
        assert_eq!(
            tail, "<https://h/api/pleroma/admin/users?offset=20>; rel=\"prev\"",
            "a short later page ends the listing"
        );

        let stepped = offset_link_header(
            base.0,
            base.1,
            base.2,
            &parse_query("limit=20&offset=10"),
            20,
        )
        .unwrap();
        assert!(
            stepped.contains("offset=0>; rel=\"prev\""),
            "one page back from 10 stops at 0, not below: {stepped}"
        );

        let filtered = offset_link_header(
            base.0,
            base.1,
            &[("domain".to_owned(), "a b".to_owned())],
            &parse_query("limit=1&offset=1"),
            1,
        )
        .unwrap();
        assert_eq!(
            filtered,
            "<https://h/api/pleroma/admin/users?domain=a+b&offset=2>; rel=\"next\", \
             <https://h/api/pleroma/admin/users?domain=a+b&offset=0>; rel=\"prev\"",
            "extras are repeated on offset links too"
        );
    }

    #[test]
    fn an_empty_or_terminal_offset_page_gets_no_link_header() {
        let params = parse_query("limit=20&offset=40");
        assert_eq!(
            offset_link_header("https://h", "/p", &[], &params, 0),
            None,
            "an empty page links nowhere"
        );
        assert_eq!(
            offset_link_header("https://h", "/p", &[], &parse_query("limit=20"), 5),
            None,
            "a short first page is the whole listing"
        );
    }

    #[test]
    fn offset_link_header_survives_a_saturated_next() {
        let params = parse_query(&format!("limit=1&offset={}", i64::MAX));
        let link = offset_link_header("https://h", "/p", &[], &params, 1).unwrap();
        assert!(
            link.contains(&format!("offset={}>; rel=\"next\"", i64::MAX)),
            "an absurd offset saturates instead of wrapping: {link}"
        );
    }

    #[test]
    fn set_link_header_writes_the_value_or_nothing() {
        let mut written = HeaderMap::new();
        set_link_header(&mut written, Some("<https://h/p?max_id=o>; rel=\"next\"")).unwrap();
        assert_eq!(
            written.get(LINK).unwrap().to_str().unwrap(),
            "<https://h/p?max_id=o>; rel=\"next\"",
            "the header is stored verbatim"
        );

        let mut untouched = HeaderMap::new();
        set_link_header(&mut untouched, None).unwrap();
        assert!(
            !untouched.contains_key(LINK),
            "an empty page sends no Link header"
        );
    }

    #[test]
    fn set_link_header_rejects_bytes_that_are_not_header_safe() {
        let mut headers = HeaderMap::new();
        let err = set_link_header(&mut headers, Some("evil\nvalue"))
            .expect_err("a newline cannot ride in a header");
        assert!(matches!(err, AppError::Internal(_)), "a server bug: {err}");
    }

    #[test]
    fn ordinal_pages_default_to_twenty_and_read_zero_as_every_row() {
        for (query, want) in [
            (None, Some(20)),
            (Some(""), Some(20)),
            (Some("limit=5"), Some(5)),
            (Some("limit=40"), Some(40)),
            (Some("limit=41"), Some(40)),
            (Some("limit=100"), Some(40)),
            (Some("limit=0"), None),
            (Some("limit=-3"), Some(20)),
            (Some("limit=abc"), Some(20)),
        ] {
            let params = OrdinalPageParams::from_query(query, MAX_LIMIT).unwrap();
            assert_eq!(params.limit, want, "`limit` of query {query:?}");
        }
    }

    #[test]
    fn ordinal_cursors_are_numbers() {
        let params =
            OrdinalPageParams::from_query(Some("max_id=9&min_id=4&since_id=7"), MAX_LIMIT).unwrap();
        assert_eq!(
            params,
            OrdinalPageParams {
                max_id: Some(9),
                min_id: Some(4),
                since_id: Some(7),
                limit: Some(20),
            },
            "the three cursors and the default page"
        );
    }

    #[test]
    fn an_ordinal_cursor_that_is_not_a_number_is_a_bad_request() {
        for value in ["", "nope", "1.5", MAX_ID] {
            for key in ["max_id", "min_id", "since_id"] {
                let query = format!("{key}={value}");
                let err = OrdinalPageParams::from_query(Some(&query), MAX_LIMIT)
                    .expect_err(&format!("{query} must be rejected"));
                assert!(
                    matches!(err, AppError::BadRequest(_)),
                    "{query} is a 400, not {err}"
                );
                assert_eq!(err.to_string(), "Invalid id", "message for {query}");
            }
        }
    }

    #[test]
    fn ordinal_keyset_orders_on_the_given_column() {
        let mut qb = QueryBuilder::new("SELECT position FROM list_members WHERE 1=1");
        let params = OrdinalPageParams::from_query(Some("max_id=9&limit=5"), MAX_LIMIT).unwrap();
        params.apply_keyset(&mut qb, "lm.position");
        assert_eq!(
            qb.into_sql(),
            "SELECT position FROM list_members WHERE 1=1 AND lm.position < $1 \
             ORDER BY lm.position DESC LIMIT $2",
            "the cursor walks to smaller positions, newest first"
        );
    }

    #[test]
    fn an_ordinal_page_without_a_limit_fetches_every_row() {
        let mut qb = QueryBuilder::new("SELECT position FROM list_members WHERE 1=1");
        OrdinalPageParams::from_query(Some("limit=0"), MAX_LIMIT)
            .unwrap()
            .apply_keyset(&mut qb, "position");
        assert_eq!(
            qb.into_sql(),
            "SELECT position FROM list_members WHERE 1=1 ORDER BY position DESC",
            "limit=0 appends no LIMIT at all"
        );
    }

    #[test]
    fn an_ordinal_min_id_page_is_read_ascending_and_flipped() {
        let mut qb = QueryBuilder::new("SELECT position FROM list_members WHERE 1=1");
        let params = OrdinalPageParams::from_query(Some("min_id=4"), MAX_LIMIT).unwrap();
        params.apply_keyset(&mut qb, "position");
        assert_eq!(
            qb.into_sql(),
            "SELECT position FROM list_members WHERE 1=1 AND position > $1 \
             ORDER BY position ASC LIMIT $2",
            "the page just after the cursor, read forward"
        );
        assert_eq!(
            params.finish(vec![4, 5, 6]),
            vec![6, 5, 4],
            "and answered newest-first"
        );
        let first = OrdinalPageParams::from_query(None, MAX_LIMIT).unwrap();
        assert_eq!(first.finish(vec![4, 5, 6]), vec![4, 5, 6], "no flip");
    }
}
