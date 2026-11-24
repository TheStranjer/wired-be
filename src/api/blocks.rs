//! The account blocks over HTTP: refusing another account, lifting that
//! refusal, and reading the caller's blocked list back.
//!
//! Pleroma's `AccountController` `block`, `unblock` and `blocks`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`, routed
//! `post "/accounts/:id/block"`, `post "/accounts/:id/unblock"` and
//! `get "/blocks"` in `lib/pleroma/web/router.ex`) over `User.block` /
//! `User.unblock` (`lib/pleroma/user.ex`) are the reference; the storage is
//! [`crate::models::block`] and the fact is this server's `blocks` table.
//!
//! All three routes require a user token ([`AuthUser`]). Scopes follow the
//! ticket: the two writes either the `follow` root or `write:blocks`, the
//! listing `read:blocks` — Pleroma's own grants for these actions
//! (`%{scopes: ["follow", "write:blocks"]}` on `block`/`unblock`,
//! `%{scopes: ["follow", "read:blocks"]}` on `blocks`; this ticket's read
//! side keeps the narrower grant). Pleroma runs the relationship actions
//! through its `relations_actions` throttle bucket (`config/config.exs`: ten
//! thousand requests per ten seconds); this server has no rate limiter yet,
//! and the ticket that adds one registers these three routes under that
//! bucket name.
//!
//! An id that is malformed, unknown or deactivated is the shared 404
//! ([`account_or_404`]), resolved before the write. `POST …/block` then runs
//! [`block::block`] — idempotent on the pair, and the one transaction that
//! severs whatever follows the two accounts held between them, counters
//! fixed — and answers the caller's [`Relationship`] to the account with
//! `blocking: true` read back off the stored row; blocking *yourself* is the
//! 422 `{"error":"Can not block yourself"}` the model states (Pleroma's
//! `AccountController` renders its `{:error, _}` as a 403, this ticket pins
//! the 422 and the wording). `POST …/unblock` runs [`block::unblock`], which
//! deletes whatever pair it is given and is content with none, so lifting a
//! block that was never there is the unchanged [`Relationship`] with 200.
//! Neither write restores what a block severed: unblocking is the fact that
//! the pair may follow each other again, not a rewind.
//!
//! `GET /api/v1/blocks` is Pleroma's `blocks`: the accounts the caller
//! blocks as [`Account`] entities, newest block first, [`PageParams`] keyset
//! pagination with Mastodon's account-list cap of 80 (default 20) over the
//! *block row id* — the [`Link`](crate::pagination::link_header) cursors
//! carry [`Block::id`], not the blocked accounts' ids — and the caller's
//! [`Relationship`] embedded in each entry. The list is the caller's own, so
//! no privacy check applies.
//!
//! Blocking is also the moment federation will owe the blocked account a
//! `Block` activity (and unblocking its `Undo`); that is the later
//! federation ticket's job, and both routes call this module's
//! `after_block_changed` hook as its documented empty landing point.
//!
//! [`block::block`]: crate::models::block::block
//! [`block::unblock`]: crate::models::block::unblock
//! [`Block::id`]: crate::models::block::Block::id

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        relationship::Relationship,
    },
    error::AppResult,
    models::{block, block::Block, user::User},
    oauth::scopes::{FOLLOW, READ_BLOCKS, WRITE_BLOCKS},
    pagination::{PageParams, link_header, set_link_header},
    state::AppState,
};

/// Page size cap of the blocked list: Mastodon's account-list limit (80),
/// above [`crate::pagination::MAX_LIMIT`] — the follow-request queue's
/// counterpart constant, with Pleroma's global 20 as the default.
const MAX_LIMIT: i64 = 80;

/// The `Link` header's query parameters carry nothing but the cursor: the
/// blocked list has no filter to repeat on either link (the relationship
/// listings' constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1` blocks router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/blocks", get(list_blocks))
        .route("/accounts/{id}/block", post(block_account))
        .route("/accounts/{id}/unblock", post(unblock_account))
}

/// `POST /api/v1/accounts/{id}/block` — block the account and answer the
/// caller's [`Relationship`] to it, `blocking: true`.
///
/// The write ([`block::block`]) is idempotent on the pair, so blocking twice
/// is one row, one `ap_id` and one answer; what it always does — including on
/// a first block — is sever the follows between the pair in both directions
/// and fix the counters they fed, in the same transaction as the block row.
/// The [`Relationship::load_many`] after the write is the render of Pleroma's
/// `relationship.json`: the answer states the stored fact rather than
/// trusting the request.
///
/// [`block::block`]: crate::models::block::block
///
/// # Errors
///
/// [`AppError::Unauthorized`](crate::error::AppError::Unauthorized) / [`AppError::Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `write:blocks | follow` check; [`AppError::NotFound`](crate::error::AppError::NotFound) for a malformed,
/// unknown or deactivated account id; [`AppError::Unprocessable`](crate::error::AppError::Unprocessable) with
/// `"Can not block yourself"` when the id names the caller. Any database
/// failure arrives through `?` as [`AppError::Database`](crate::error::AppError::Database).
async fn block_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_BLOCKS, FOLLOW])?;
    let target = account_or_404(&state, &id).await?;
    let block = block::block(&state.db, &state.config.base_url, &auth.0, &target).await?;
    debug!(
        block = %block.id,
        user = %auth.0.id,
        target = %target.id,
        "account blocked"
    );
    after_block_changed(&state, &block, true).await;
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, target.id).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unblock` — lift the caller's block on the
/// account and answer their [`Relationship`] to it, `blocking: false`.
///
/// The withdrawal ([`block::unblock`]) deletes whatever pair it is given and
/// is content with none, so unblocking an account that was never blocked (or
/// already lifted) changes nothing and is answered the unchanged
/// [`Relationship`] with 200. What it does not do is restore the follows the
/// block severed: the pair starts from nothing again.
///
/// [`block::unblock`]: crate::models::block::unblock
///
/// # Errors
///
/// Those of [`block_account`] minus the self guard: an id naming the caller
/// is a pair no block row can exist for, so it answers the unchanged
/// [`Relationship`] like any other unblocked account. Any database failure
/// arrives through `?` as [`AppError::Database`](crate::error::AppError::Database).
async fn unblock_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_BLOCKS, FOLLOW])?;
    let target = account_or_404(&state, &id).await?;
    let removed = block::unblock(&state.db, &auth.0, &target).await?;
    if let Some(block) = removed.as_ref() {
        debug!(
            block = %block.id,
            user = %auth.0.id,
            target = %target.id,
            "block withdrawn"
        );
        after_block_changed(&state, block, false).await;
    }
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, target.id).await?,
    ))
}

/// `GET /api/v1/blocks` — the accounts the caller blocks, newest block
/// first, one [`PageParams`] page at a time (default limit 20, at most
/// [`MAX_LIMIT`]) under a `Link` header keyed on the block row ids. The list
/// is the caller's own, so no privacy check applies.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`](crate::error::AppError::Unauthorized) from [`AuthUser`] without a valid user token;
/// [`AppError::Forbidden`](crate::error::AppError::Forbidden) when the token does not grant `read:blocks`;
/// [`AppError::BadRequest`](crate::error::AppError::BadRequest) from [`PageParams`] for a malformed cursor. Any
/// database failure arrives through `?` as [`AppError::Database`](crate::error::AppError::Database), and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`](crate::error::AppError::Internal).
async fn list_blocks(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require(READ_BLOCKS)?;
    let page = PageParams::from_query(uri.query(), MAX_LIMIT)?;
    let entries = block::blocking_page(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "blocked accounts listed"
    );
    let viewer: AccountViewer<'_> = Some(&auth.0);
    page_response(&state, viewer, &entries, uri.path()).await
}

/// Renders one fetched blocked page for `viewer`: every entry's [`User`] as
/// an [`Account`] with the viewer's relationship embedded where one is
/// preloaded (see [`Relationship::load_map`]), under the `Link` header
/// [`page_link`] derives from the block row ids.
async fn page_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[(Block, User)],
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let targets: Vec<Uuid> = entries.iter().map(|(_, blocked)| blocked.id).collect();
    let relationships = Relationship::load_map(&state.db, viewer, &targets).await?;
    let link = page_link(&state.config.base_url, path, entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = entries
        .iter()
        .map(|(_, blocked)| {
            Account::from_user_for(viewer, blocked, &state.config, relationships.as_ref())
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// The `Link` header value for a fetched blocked page: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`Block::id`] — the cursors key on the `blocks` row, not on the blocked
/// accounts — and `None` for an empty page.
///
/// [`Block::id`]: crate::models::block::Block::id
fn page_link(base_url: &str, path: &str, entries: &[(Block, User)]) -> Option<String> {
    let newest = entries.first()?.0.id.to_string();
    let oldest = entries.last()?.0.id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

/// Post-write hook of both block routes: hands the write to
/// [`crate::services::federation_out`], which sends the `Block` activity
/// (and its `Undo`, when a block is lifted) to the blocked account's
/// instance — the activity to send is [`Block::ap_id`], the pair is
/// [`Block::blocker_id`] and [`Block::blocked_id`], and the queueing happens
/// through `state` (decision 11).
///
/// `blocking` states which way the write moved the fact: `true` for the
/// `Block` a `…/block` left behind, `false` for the `Undo` of the `Block` a
/// `…/unblock` removed. The write is what this hook reports, so an unblock
/// that found no row to remove never reaches it; `…/block` is idempotent on
/// the pair, so a repeat restates the same fact and the federation service
/// deduplicates the delivery per activity and inbox.
///
/// The hook never fails the request: federation is the write's echo, and an
/// echo that cannot be queued is logged, not answered.
///
/// [`Block::ap_id`]: crate::models::block::Block::ap_id
/// [`Block::blocker_id`]: crate::models::block::Block::blocker_id
/// [`Block::blocked_id`]: crate::models::block::Block::blocked_id
pub(crate) async fn after_block_changed(state: &AppState, block: &Block, blocking: bool) {
    crate::services::federation_out::after_block_changed(state, block, !blocking).await;
}
