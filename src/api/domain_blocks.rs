//! The user domain blocks over HTTP: `GET`, `POST` and `DELETE`
//! `/api/v1/domain_blocks`.
//!
//! Pleroma's `DomainBlockController`
//! (`lib/pleroma/web/mastodon_api/controllers/domain_block_controller.ex`)
//! is the reference: the listing answers the blocked domains as a plain
//! array of strings, and both writes answer the empty object `{}` —
//! including when the fact was already true (Pleroma's `block_domain` /
//! `unblock_domain` are idempotent, and so are the rows behind
//! [`domain_block`]'s writes).
//!
//! All three routes require a user token ([`AuthUser`]). Scopes follow
//! the ticket: the listing needs `read:blocks` (which the broad `read`
//! covers), the two writes either `write:blocks` or the `follow` root —
//! Pleroma's alternative grant for this controller.
//!
//! `GET` pages like the relationship listings: [`PageParams`] keyset
//! pagination (default limit 20, at most
//! [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)) over the *block row id*,
//! so the [`Link`](crate::pagination::link_header) header's
//! `max_id`/`min_id` carry [`DomainBlock::id`](crate::models::domain_block::DomainBlock::id)
//! and the newest block is answered first. Pleroma has no pagination
//! here (its blocks are an array on the user row); the cursors are this
//! table's shape of the same listing.
//!
//! Both writes take a `domain` parameter, read through [`ApiParams`] so
//! it arrives in the query string, a form body or a JSON body alike
//! (decision 10; Mastodon clients use all three, and Pleroma's
//! `replace_params: false` keeps the query visible too). The value is
//! canonicalised by
//! [`normalize_domain`](crate::models::domain_block::normalize_domain):
//! absent or blank is the 400 `"Missing parameter: domain"`, an input
//! that leaves no dotted domain the 422 `"Invalid domain"`, and anything
//! else is stored under its one canonical spelling, so `@Foo.example`
//! and `https://foo.example/` are the same block.

use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    http::HeaderMap,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;

use crate::{
    api::accounts::after_follow_changed,
    auth::AuthUser,
    error::{AppError, AppResult},
    extract::ApiParams,
    models::{
        domain_block::{self, DomainBlock},
        follow,
    },
    oauth::scopes::{FOLLOW, READ_BLOCKS, WRITE_BLOCKS},
    pagination::{PageParams, link_header, set_link_header},
    services::federation_out,
    state::AppState,
};

/// 400 body when a write names no `domain` (Mastodon's
/// `ParameterMissing` wording, which the ticket pins for this endpoint).
const MISSING_DOMAIN: &str = "Missing parameter: domain";
/// 422 body when the `domain` normalises to nothing blockable.
const INVALID_DOMAIN: &str = "Invalid domain";

/// The `Link` header's query parameters carry nothing but the cursor:
/// the listing has no filter to repeat on either link (the relationship
/// listings' constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1/domain_blocks` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/domain_blocks",
        get(list_domain_blocks)
            .post(create_domain_block)
            .delete(delete_domain_block),
    )
}

/// The request parameters of both writes: Pleroma's `%{"domain" =>
/// domain}` pattern match, which an absent field simply fails — the
/// "missing" case is the endpoint's own 400, so the field is optional
/// here (the `lookup` handler's pattern).
#[derive(Debug, Deserialize)]
struct DomainParams {
    /// The domain to block or unblock, in any spelling
    /// [`normalize_domain`](domain_block::normalize_domain) accepts.
    domain: Option<String>,
}

/// `GET /api/v1/domain_blocks` — the caller's blocked domains as a JSON
/// array of strings, newest block first, one [`PageParams`] page at a
/// time under a `Link` header keyed on the block row ids.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to
/// `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:blocks`; [`AppError::BadRequest`] from [`PageParams`] for a
/// malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_domain_blocks(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<String>>)> {
    auth.require(READ_BLOCKS)?;
    let entries = domain_block::domains_page(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "domain blocks listed"
    );
    let link = page_link(&state.config.base_url, uri.path(), &entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let domains = entries.into_iter().map(|entry| entry.domain).collect();
    Ok((headers, Json(domains)))
}

/// `POST /api/v1/domain_blocks` — block a domain for the caller and
/// answer `{}`. Blocking what is already blocked is one more `{}` over
/// the same row (Pleroma's `Enum.member?` guard, this table's unique
/// key), and the canonical spelling means every client-side spelling of
/// a domain lands on it.
///
/// Blocking also severs every follow between the caller and the remote
/// accounts on the domain, as Mastodon does: followers there are removed
/// (a `Reject` of their stored `Follow` goes to their inbox), the caller's
/// follows of accounts there are withdrawn (an `Undo Follow`), and pending
/// requests either way are dropped. Unblocking restores nothing, and a
/// repeated block finds nothing left to sever.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:blocks | follow` check; [`AppError::BadRequest`] when
/// `domain` is absent or blank; [`AppError::Unprocessable`] when it is no
/// blockable domain. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn create_domain_block(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<DomainParams>,
) -> AppResult<Json<Map<String, Value>>> {
    auth.require_any(&[WRITE_BLOCKS, FOLLOW])?;
    let domain = requested_domain(params.domain.as_deref())?;
    domain_block::block_domain(&state.db, auth.0.id, &domain).await?;
    let severed = follow::sever_with_domain(&state.db, auth.0.id, &domain).await?;
    debug!(
        domain = %domain,
        user = %auth.0.id,
        followers = severed.followers.len(),
        followees = severed.followees.len(),
        "domain blocked"
    );
    for (removed, _follower) in &severed.followers {
        federation_out::after_follower_removed(&state, &auth.0, removed).await;
    }
    for (removed, target) in &severed.followees {
        after_follow_changed(&state, &auth.0, target, Some(removed), false).await;
    }
    Ok(empty_answer())
}

/// `DELETE /api/v1/domain_blocks` — lift the caller's block on a domain
/// and answer `{}`. The `domain` is accepted in the query, a form body
/// or a JSON body alike ([`ApiParams`]); an unblocked or never blocked
/// domain is one more `{}` (Pleroma's `delete_from_array` no-op, this
/// model's idempotent [`domain_block::unblock_domain`]).
///
/// # Errors
///
/// Exactly those of [`create_domain_block`].
async fn delete_domain_block(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<DomainParams>,
) -> AppResult<Json<Map<String, Value>>> {
    auth.require_any(&[WRITE_BLOCKS, FOLLOW])?;
    let domain = requested_domain(params.domain.as_deref())?;
    domain_block::unblock_domain(&state.db, auth.0.id, &domain).await?;
    debug!(
        domain = %domain,
        user = %auth.0.id,
        "domain unblocked"
    );
    Ok(empty_answer())
}

/// The `domain` a write must name: absent or blank is the 400
/// [`MISSING_DOMAIN`], an input that normalises to no domain the 422
/// [`INVALID_DOMAIN`], and anything else its canonical spelling — the
/// only text [`domain_block`] ever stores.
///
/// # Errors
///
/// [`AppError::BadRequest`] / [`AppError::Unprocessable`] as described.
fn requested_domain(raw: Option<&str>) -> AppResult<String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Err(AppError::BadRequest(MISSING_DOMAIN.to_owned()));
    };
    domain_block::normalize_domain(raw)
        .ok_or_else(|| AppError::Unprocessable(INVALID_DOMAIN.to_owned()))
}

/// The `{}` both writes answer — Pleroma's `json(conn, %{})`.
fn empty_answer() -> Json<Map<String, Value>> {
    Json(Map::new())
}

/// The `Link` header value for a fetched page: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`DomainBlock::id`] — the cursors key on the block row, not on the
/// domain — and `None` for an empty page.
fn page_link(base_url: &str, path: &str, entries: &[DomainBlock]) -> Option<String> {
    let newest = entries.first()?.id.to_string();
    let oldest = entries.last()?.id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}
