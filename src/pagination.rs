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
use sqlx::{
    FromRow, PgPool, QueryBuilder,
    postgres::{PgRow, Postgres},
};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    util::id::parse_id,
};

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

/// Runs one keyset page of the statement `builder` holds, paged on `column`.
///
/// Appends `page`'s cursors, ordering and `LIMIT` with
/// [`PageParams::apply_keyset`], fetches every row as `T` and hands them
/// through [`PageParams::finish`], so a `min_id` page comes back flipped to
/// newest-first like every other.
///
/// The builder must already end in a `WHERE` predicate the cursors can
/// continue with `AND` (callers push their own filters before the call).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`].
pub async fn keyset_page<T>(
    pool: &PgPool,
    mut builder: QueryBuilder<Postgres>,
    column: &str,
    page: &PageParams,
) -> AppResult<Vec<T>>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    page.apply_keyset(&mut builder, column);
    let rows = builder.build_query_as::<T>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

/// [`keyset_page`] over a `stem` that ends at a placeholder bound to `bound`.
///
/// Builds the [`QueryBuilder`] over `stem`, binds `bound` as that
/// placeholder, then pages on `column`.
///
/// # Errors
///
/// As [`keyset_page`].
pub async fn keyset_page_by<T>(
    pool: &PgPool,
    stem: &str,
    bound: Uuid,
    column: &str,
    page: &PageParams,
) -> AppResult<Vec<T>>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    let mut builder = QueryBuilder::<Postgres>::new(stem);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(bound);
    keyset_page(pool, builder, column, page).await
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
mod tests;
