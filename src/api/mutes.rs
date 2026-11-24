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
    api::helpers::{AccountPageEntry, account_or_404, account_page},
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        relationship::Relationship,
    },
    error::AppResult,
    extract::{ApiParams, flex},
    models::{mute, mute::Mute, user::User},
    oauth::scopes::{FOLLOW, READ_MUTES, WRITE_MUTES},
    pagination::{ACCOUNT_LIST_MAX_LIMIT, PageParams},
    state::AppState,
};

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
/// [`AppError::Unauthorized`](crate::error::AppError::Unauthorized) / [`AppError::Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `write:mutes | follow` check; [`AppError::NotFound`](crate::error::AppError::NotFound) for a malformed,
/// unknown or deactivated account id; [`AppError::Unprocessable`](crate::error::AppError::Unprocessable) with
/// `"Can not mute yourself"` when the id names the caller, or with the
/// extractor's message when `notifications` or `duration` is not the value it
/// asks for. Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError::Database).
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
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, target.id).await?,
    ))
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
/// failure arrives through `?` as [`AppError::Database`](crate::error::AppError::Database).
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
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, target.id).await?,
    ))
}

/// `GET /api/v1/mutes` — the accounts the caller mutes, newest mute first, one
/// [`PageParams`] page at a time (default limit 20, at most [`ACCOUNT_LIST_MAX_LIMIT`])
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
/// [`AppError::Unauthorized`](crate::error::AppError::Unauthorized) from [`AuthUser`] without a valid user token;
/// [`AppError::Forbidden`](crate::error::AppError::Forbidden) when the token does not grant `read:mutes`;
/// [`AppError::BadRequest`](crate::error::AppError::BadRequest) from [`PageParams`] for a malformed cursor. Any
/// database failure arrives through `?` as [`AppError::Database`](crate::error::AppError::Database), and
/// [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value that is not header-safe as
/// [`AppError::Internal`](crate::error::AppError::Internal).
async fn list_mutes(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require(READ_MUTES)?;
    let page = PageParams::from_query(uri.query(), ACCOUNT_LIST_MAX_LIMIT)?;
    let entries = mute::muting_page(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "muted accounts listed"
    );
    let viewer: AccountViewer<'_> = Some(&auth.0);
    account_page(&state, viewer, uri.path(), &entries).await
}

/// A muted-list row: the cursors key on the `mutes` row ([`Mute::id`]), not
/// on the muted account, and every entry carries its mute's expiry as
/// `mute_expires_at` ([`Account::with_mute_expires_at`]).
impl AccountPageEntry for (Mute, User) {
    fn cursor(&self) -> Uuid {
        self.0.id
    }

    fn user(&self) -> &User {
        &self.1
    }

    fn decorate(&self, account: Account) -> Account {
        account.with_mute_expires_at(self.0.expires_at)
    }
}
