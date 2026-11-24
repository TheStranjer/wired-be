//! The media-proxy cache ledger routes: the listing, the delete and the
//! purge, with the optional `PURGE` invalidation behind them.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Query, State},
    http::Method,
};
use serde::Deserialize;
use tracing::{debug, warn};

use super::{
    AREA_CACHE, CLASS_READ, CLASS_WRITE, DEFAULT_CACHE_PAGE_SIZE, ERROR_STATUS_MIN,
    INVALID_PARAMETERS, PURGE_METHOD,
};
use crate::{
    auth::admin::AdminUser,
    entities::admin::{MediaProxyCachePage, MediaProxyCacheUrls},
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::media_proxy_cache,
    services::media_proxy,
    state::AppState,
};

/// Most response-body bytes read from the cache's `PURGE` answer: only the
/// status matters, so 64 KiB is ample.
const PURGE_RESPONSE_BYTES: usize = 64 * 1024;

/// `GET /api/v1/pleroma/admin/media_proxy_caches` — Pleroma's `index`:
/// the filtered ledger windowed by `page` / `page_size` (defaults 1 / 20,
/// capped like the rest of the admin listings), answered as
/// `{urls, page_size, count}` with `count` the whole filtered total.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the two reads.
pub(super) async fn list_caches(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<MediaProxyCachePage>> {
    admin.require(CLASS_READ, AREA_CACHE)?;
    let query = super::super::text_of(&params, "query");
    let page = super::super::page_number(&params, "page", super::super::DEFAULT_PAGE);
    let size = cache_page_size(&params);
    let offset = (page - 1).saturating_mul(size);
    let count = media_proxy_cache::count(&state.db, query.as_deref()).await?;
    let urls = media_proxy_cache::list(&state.db, query.as_deref(), size, offset).await?;
    debug!(
        shown = urls.len(),
        count, page, "admin media proxy cache listed"
    );
    Ok(Json(MediaProxyCachePage {
        urls,
        page_size: size,
        count,
    }))
}

/// The cache listing's `page_size`: the admin lists' lenient reading with
/// the ticket's 20-row default and the shared [`super::super::MAX_PAGE_SIZE`] cap.
fn cache_page_size(params: &HashMap<String, String>) -> i64 {
    super::super::page_number_of(
        super::super::text_of(params, "page_size").as_deref(),
        DEFAULT_CACHE_PAGE_SIZE,
    )
    .min(super::super::MAX_PAGE_SIZE)
}

/// The body of both cache write actions: Pleroma's required `urls` array
/// and the purge's optional `ban` flag, read as `Option`s so the missing
/// `urls` answers the ticket's own 400 and not the extractor's 422.
#[derive(Debug, Deserialize)]
pub(super) struct CacheWriteBody {
    /// The URLs the action takes; absence is the refusal, an empty list
    /// an action on nothing.
    #[serde(default)]
    urls: Option<Vec<String>>,
    /// The purge's ban flag, accepted through [`flex::opt_bool`] so a
    /// form-encoded `"true"` counts. This ledger records served URLs and
    /// has no ban state to carry, so the flag changes nothing.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    ban: Option<bool>,
}

/// `POST /api/v1/pleroma/admin/media_proxy_caches/delete` — Pleroma's
/// `delete`: remove the URLs from the ledger, issuing the same optional
/// `PURGE` invalidation [`purge_caches`] does, and answer
/// `{"urls":[...]}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] with
/// [`INVALID_PARAMETERS`] for a body without `urls`;
/// [`AppError::Database`] from the delete.
pub(super) async fn delete_caches(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<CacheWriteBody>,
) -> AppResult<Json<MediaProxyCacheUrls>> {
    admin.require(CLASS_WRITE, AREA_CACHE)?;
    let urls = require_urls(body.urls)?;
    let removed = clear_cache(&state, &urls).await?;
    debug!(
        requested = urls.len(),
        removed, "admin media proxy cache deleted"
    );
    Ok(Json(MediaProxyCacheUrls::new(&urls)))
}

/// `POST /api/v1/pleroma/admin/media_proxy_caches/purge` — Pleroma's
/// `purge`: a `PURGE` invalidation per URL, the URLs out of the ledger,
/// `{"urls":[...]}` back. `ban` is read for API shape and changes nothing
/// (see [`CacheWriteBody::ban`]).
///
/// # Errors
///
/// As [`delete_caches`]'s — the purge requests themselves never fail the
/// action, they only reach the log.
pub(super) async fn purge_caches(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<CacheWriteBody>,
) -> AppResult<Json<MediaProxyCacheUrls>> {
    admin.require(CLASS_WRITE, AREA_CACHE)?;
    let urls = require_urls(body.urls)?;
    if body.ban.unwrap_or(false) {
        debug!("admin media proxy cache purge asked to ban: this ledger only records served urls");
    }
    let removed = clear_cache(&state, &urls).await?;
    debug!(
        requested = urls.len(),
        removed, "admin media proxy cache purged"
    );
    Ok(Json(MediaProxyCacheUrls::new(&urls)))
}

/// The `urls` half of the ticket: absent (or JSON `null`) is the 400
/// `{"error":"Invalid parameters"}`, present passes through.
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`INVALID_PARAMETERS`].
fn require_urls(urls: Option<Vec<String>>) -> AppResult<Vec<String>> {
    urls.ok_or_else(|| AppError::BadRequest(INVALID_PARAMETERS.to_owned()))
}

/// The shared delete/purge write: first the optional `PURGE` requests,
/// then the ledger removal. Pleroma's `remove_from_banned_urls` and
/// `Invalidation.purge` in the one order that keeps a refused purge from
/// resurrecting an entry.
///
/// # Errors
///
/// [`AppError::Database`] from the delete.
async fn clear_cache(state: &AppState, urls: &[String]) -> AppResult<u64> {
    for url in urls {
        send_purge(state, url).await;
    }
    media_proxy_cache::remove(&state.db, urls).await
}

/// One URL's `PURGE` invalidation, when `MEDIA_PROXY_PURGE_URL` names the
/// origin and the proxy signs links ([`purge_target`]): the request is
/// best-effort — the ticket logs failures and moves on, because the
/// admin's ledger write must not hinge on a cache they pointed at.
///
/// [`purge_target`]: media_proxy::purge_target
async fn send_purge(state: &AppState, url: &str) {
    let Some(target) = media_proxy::purge_target(&state.config, url) else {
        return;
    };
    let Ok(method) = Method::from_bytes(PURGE_METHOD) else {
        warn!(%target, "media proxy purge: PURGE is not a usable HTTP method");
        return;
    };
    match state
        .http_client
        .request(method, &target, &[], PURGE_RESPONSE_BYTES)
        .await
    {
        Ok(response) if response.status < ERROR_STATUS_MIN => {
            debug!(status = response.status, %target, "media proxy cache purged");
        }
        Ok(response) => {
            warn!(status = response.status, %target, "media proxy purge request refused");
        }
        Err(err) => {
            warn!(%err, %target, "media proxy purge request failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_cache_body_reads_the_ticket_shape() {
        let full: CacheWriteBody = serde_json::from_value(json!({
            "urls": ["https://remote.example/a.png"],
            "ban": "true",
        }))
        .expect("the ticket's shape deserializes");
        assert_eq!(
            full.urls.as_deref(),
            Some(&["https://remote.example/a.png".to_owned()][..])
        );
        assert_eq!(full.ban, Some(true), "the form spelling reads through flex");
        let empty: CacheWriteBody =
            serde_json::from_value(json!({})).expect("an empty body deserializes");
        assert_eq!(
            empty.urls, None,
            "no urls is the handler's refusal, not the extractor's"
        );
        let zeroed: CacheWriteBody =
            serde_json::from_value(json!({"urls": []})).expect("the empty list deserializes");
        assert_eq!(
            require_urls(zeroed.urls)
                .expect("an empty list is an action on nothing")
                .len(),
            0,
            "and passes the gate"
        );
    }

    #[test]
    fn the_missing_urls_are_the_pinned_400() {
        let err = require_urls(None).expect_err("absence is refused");
        assert_eq!(err.to_string(), INVALID_PARAMETERS, "the ticket wording");
    }

    #[test]
    fn the_cache_page_size_defaults_to_the_ticket_window() {
        let parse = |pairs: &[(&str, &str)]| {
            let params: HashMap<String, String> = pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            cache_page_size(&params)
        };
        assert_eq!(parse(&[]), DEFAULT_CACHE_PAGE_SIZE, "absent is 20");
        assert_eq!(parse(&[("page_size", "nope")]), 20, "junk defaults");
        assert_eq!(parse(&[("page_size", "3")]), 3, "a usable size passes");
        assert_eq!(
            parse(&[("page_size", "5000")]),
            super::super::super::MAX_PAGE_SIZE,
            "and the admin cap holds"
        );
    }
}
