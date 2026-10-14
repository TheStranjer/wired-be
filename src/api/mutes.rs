//! The account mutes over HTTP: hiding another account from the caller's
//! timelines, lifting that, and reading the caller's muted list back.
//!
//! Pleroma's `AccountController` `mute`, `unmute` and `mutes`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`, routed
//! `post "/accounts/:id/mute"`, `post "/accounts/:id/unmute"` and
//! `get "/mutes"` in `lib/pleroma/web/router.ex`) over `User.mute` /
//! `User.unmute` (`lib/pleroma/user.ex`) are the reference; the storage is
//! [`crate::models::mute`] and the fact is this server's `mutes` table.
//!
//! All three routes require a user token ([`AuthUser`]). Scopes follow the
//! ticket: the two writes either the `follow` root or `write:mutes`, the
//! listing `read:mutes` — Pleroma's own grants for these actions
//! (`%{scopes: ["follow", "write:mutes"]}` on `mute`/`unmute`,
//! `%{scopes: ["follow", "read:mutes"]}` on `mutes`; this ticket's read side
//! keeps the narrower grant). Pleroma runs the relationship actions through
//! its `relations_actions` throttle bucket (`config/config.exs`: ten thousand
//! requests per ten seconds); this server has no rate limiter yet, and the
//! ticket that adds one registers these three routes under that bucket name.
//!
//! An id that is malformed, unknown or deactivated is the shared 404
//! ([`account_or_404`]), resolved before the write. `POST …/mute` then reads
//! its two Mastodon parameters through [`ApiParams`] — `notifications`
//! (default `true`) and `duration` in seconds, where `0` or an absent value
//! means an indefinite mute — and runs [`mute::mute`], an upsert on the pair
//! that keeps the stored row and its id and rewrites the two options. It
//! answers the caller's [`Relationship`] to the account with `muting: true`
//! and `muting_notifications` as the request's `notifications` parameter
//! stored them, both read back off the row rather than trusted from the
//! request. Muting *yourself* is the 422 `{"error":"Can not mute yourself"}`
//! the model states (Pleroma's `AccountController` renders its `{:error, _}`
//! as a 403, this ticket pins the 422 and the wording); `duration` that is not
//! a number is the extractor's 422. `POST …/unmute` runs [`mute::unmute`],
//! which deletes whatever pair it is given and is content with none, so
//! lifting a mute that was never there is the unchanged [`Relationship`] with
//! 200. Neither write touches the pair's follows: a mute hides, it does not
//! sever (contrast [`crate::models::block`]).
//!
//! `GET /api/v1/mutes` is Pleroma's `mutes`: the accounts the caller mutes as
//! [`Account`] entities, newest mute first, [`PageParams`] keyset pagination
//! with Mastodon's account-list cap of 80 (default 20) over the *mute row id*
//! — the [`Link`](crate::pagination::link_header) cursors carry
//! [`Mute::id`], not the muted accounts' ids — and the caller's
//! [`Relationship`] embedded in each entry. Every entry carries the extra
//! top-level `mute_expires_at` key of Mastodon's mutes listing: the mute's
//! expiry in the Mastodon time format, or `null` for an indefinite mute.
//! A mute whose duration has run out is on no page and in no flag, whether or
//! not the `mutes.expire` sweep has deleted its row yet. The list is the
//! caller's own, so no privacy check applies.
//!
//! [`Mute::id`]: crate::models::mute::Mute::id

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    extract::{ApiParams, flex},
    models::{mute, mute::Mute, user::User},
    oauth::scopes::{FOLLOW, READ_MUTES, WRITE_MUTES},
    pagination::{PageParams, link_header, set_link_header},
    state::AppState,
};

/// Page size cap of the muted list: Mastodon's account-list limit (80), above
/// [`crate::pagination::MAX_LIMIT`] — the blocked list's counterpart constant,
/// with Pleroma's global 20 as the default.
const MAX_LIMIT: i64 = 80;

/// The `Link` header's query parameters carry nothing but the cursor: the
/// muted list has no filter to repeat on either link (the relationship
/// listings' constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1` mutes router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mutes", get(list_mutes))
        .route("/accounts/{id}/mute", post(mute_account))
        .route("/accounts/{id}/unmute", post(unmute_account))
}

/// The two Mastodon parameters of `POST …/mute`, read from the query string,
/// a form body or a JSON body alike ([`ApiParams`], decision 10; Mastodon
/// clients use all three). Both are optional: the endpoint applies the
/// parameter defaults, which [`MuteParams::into_options`] states.
#[derive(Debug, Deserialize)]
struct MuteParams {
    /// Mastodon's `notifications`: whether the mute also hides the muted
    /// account's notifications. Pleroma's `mute` reads it the same way
    /// (`Map.get(params, :notifications, true)`), and a form's `"true"`/`"1"`
    /// spells the same thing through [`flex::opt_bool`].
    #[serde(default, deserialize_with = "flex::opt_bool")]
    notifications: Option<bool>,
    /// Mastodon's `duration`, in seconds from the write: `0` or absent mutes
    /// indefinitely (Pleroma's `Map.get(params, :duration, 0)`, which only
    /// stores an expiry when the number is positive).
    #[serde(default, deserialize_with = "flex::opt_i64")]
    duration: Option<i64>,
}

impl MuteParams {
    /// The model's options: both parameters defaulted as Mastodon documents
    /// them, with `duration=0` folded into "no expiry" here so the model never
    /// sees a duration it would have to second-guess.
    fn into_options(self) -> mute::MuteOptions {
        mute::MuteOptions {
            notifications: self.notifications.unwrap_or(true),
            expires_in: self.duration.filter(|secs| *secs > 0),
        }
    }
}

/// `POST /api/v1/accounts/{id}/mute` — mute the account and answer the
/// caller's [`Relationship`] to it, `muting: true`.
///
/// The write ([`mute::mute`]) is an upsert on the pair, so muting twice is one
/// row, one id and one answer, and the second call's `notifications` and
/// `duration` are what the row ends up storing. Nothing between the pair's
/// follows changes: unlike a block, a mute severs nothing. The
/// [`Relationship::load_many`] after the write is the render of Pleroma's
/// `relationship.json`: the answer states the stored fact rather than trusting
/// the request.
///
/// [`mute::mute`]: crate::models::mute::mute
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:mutes | follow` check; [`AppError::NotFound`] for a malformed,
/// unknown or deactivated account id; [`AppError::Unprocessable`] with
/// `"Can not mute yourself"` when the id names the caller, or with the
/// extractor's message when `notifications` or `duration` is not the value it
/// asks for. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn mute_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<MuteParams>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_MUTES, FOLLOW])?;
    let target = account_or_404(&state, &id).await?;
    let options = params.into_options();
    let row = mute::mute(&state.db, &auth.0, &target, options).await?;
    debug!(
        mute = %row.id,
        user = %auth.0.id,
        target = %target.id,
        notifications = options.notifications,
        timed = row.expires_at.is_some(),
        "account muted"
    );
    one_relationship(&state, auth.0.id, target.id).await
}

/// `POST /api/v1/accounts/{id}/unmute` — lift the caller's mute on the account
/// and answer their [`Relationship`] to it, `muting: false`.
///
/// The withdrawal ([`mute::unmute`]) deletes whatever pair it is given and is
/// content with none, so unmuting an account that was never muted (or already
/// lifted, or whose row the expiry sweep has already taken) changes nothing and
/// is answered the unchanged [`Relationship`] with 200.
///
/// [`mute::unmute`]: crate::models::mute::unmute
///
/// # Errors
///
/// Those of [`mute_account`] minus the self guard and the parameters: an id
/// naming the caller is a pair no mute row can exist for, so it answers the
/// unchanged [`Relationship`] like any other unmuted account. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn unmute_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_MUTES, FOLLOW])?;
    let target = account_or_404(&state, &id).await?;
    let removed = mute::unmute(&state.db, &auth.0, &target).await?;
    if let Some(row) = removed.as_ref() {
        debug!(
            mute = %row.id,
            user = %auth.0.id,
            target = %target.id,
            "mute withdrawn"
        );
    }
    one_relationship(&state, auth.0.id, target.id).await
}

/// `GET /api/v1/mutes` — the accounts the caller mutes, newest mute first, one
/// [`PageParams`] page at a time (default limit 20, at most [`MAX_LIMIT`])
/// under a `Link` header keyed on the mute row ids, every entry carrying its
/// mute's expiry as `mute_expires_at`. Lapsed mutes are not on the page. The
/// list is the caller's own, so no privacy check applies.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user token;
/// [`AppError::Forbidden`] when the token does not grant `read:mutes`;
/// [`AppError::BadRequest`] from [`PageParams`] for a malformed cursor. Any
/// database failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
async fn list_mutes(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require(READ_MUTES)?;
    let page = PageParams::from_query(uri.query(), MAX_LIMIT)?;
    let entries = mute::muting_page(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "muted accounts listed"
    );
    let viewer: AccountViewer<'_> = Some(&auth.0);
    page_response(&state, viewer, &entries, uri.path()).await
}

/// The caller's [`Relationship`] to one account, rendered after a mute write:
/// [`Relationship::load_many`] over the single target, so the answer states the
/// stored fact rather than trusting the request (the write-behind read of
/// Pleroma's `relationship.json`, the shape [`crate::api::blocks`] uses for its
/// writes).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`]; the
/// [`AppError::Internal`] arm only documents that the loader answers one row per
/// requested id, which a single-target batch always does.
async fn one_relationship(
    state: &AppState,
    viewer_id: Uuid,
    target_id: Uuid,
) -> AppResult<Json<Relationship>> {
    let loaded = Relationship::load_many(&state.db, viewer_id, &[target_id]).await?;
    let relationship = loaded.into_iter().next().ok_or_else(|| {
        AppError::Internal("the relationship loader skipped its only target".to_owned())
    })?;
    Ok(Json(relationship))
}

/// Renders one fetched muted page for `viewer`: every entry's [`User`] as an
/// [`Account`] with the viewer's relationship embedded where one is preloaded
/// (see [`preload_page_relationships`]) and its mute's expiry attached as
/// `mute_expires_at` (see [`Account::with_mute_expires_at`]), under the `Link`
/// header [`page_link`] derives from the mute row ids.
async fn page_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[(Mute, User)],
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let relationships = preload_page_relationships(state, viewer, entries).await?;
    let link = page_link(&state.config.base_url, path, entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = entries
        .iter()
        .map(|(row, muted)| {
            Account::from_user_for(viewer, muted, &state.config, relationships.as_ref())
                .with_mute_expires_at(row.expires_at)
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// The [`RelationshipMap`] for one muted page: the viewer's relationship to
/// every listed account in a single batched [`Relationship::load_many`], keyed
/// by target id (which is how [`Account::from_user_for`] reads its preload).
async fn preload_page_relationships(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[(Mute, User)],
) -> AppResult<Option<RelationshipMap>> {
    let Some(who) = viewer else {
        return Ok(None);
    };
    let targets: Vec<Uuid> = entries.iter().map(|(_, muted)| muted.id).collect();
    let loaded = Relationship::load_many(&state.db, who.id, &targets).await?;
    let map = loaded
        .into_iter()
        .zip(targets)
        .map(|(relationship, target)| (target, relationship))
        .collect();
    Ok(Some(map))
}

/// The `Link` header value for a fetched muted page: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`Mute::id`] — the cursors key on the `mutes` row, not on the muted accounts
/// — and `None` for an empty page.
///
/// [`Mute::id`]: crate::models::mute::Mute::id
fn page_link(base_url: &str, path: &str, entries: &[(Mute, User)]) -> Option<String> {
    let newest = entries.first()?.0.id.to_string();
    let oldest = entries.last()?.0.id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}
