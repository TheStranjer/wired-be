//! The read endpoints for accounts: `GET /api/v1/accounts/verify_credentials`,
//! `GET /api/v1/accounts/{id}`, `…/lookup`, `…/relationships` and the
//! relationship listings `…/{id}/followers` and `…/{id}/following`.
//!
//! Pleroma's `AccountController.verify_credentials`, `.show`, `.lookup`,
//! `.relationships`, `.followers` and `.following`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`)
//! are the reference.
//!
//! All of these answer the core [`Account`] entity
//! (decision 3: rendered from the row by [`Account::from_user_for`]); they
//! serve local and remote mirrors alike (decision 7) and treat an
//! account that is unknown, unresolvable or deactivated as a 404 "Record
//! not found" — the answer Pleroma's `visible_for` check renders through
//! its fallback controller.
//!
//! `verify_credentials` is the account's own view of itself: [`AuthUser`]
//! plus the `read:accounts` scope, and the entity rendered with the
//! caller as its [`AccountViewer`], so the owner-only keys of the
//! `pleroma` block are in and nothing is hidden. Its additions over `show`
//! are the [`source`](crate::entities::account::AccountSource) object — the
//! raw note and fields a client edits, the posting defaults, and the size
//! of the pending follow-request queue, the one value here that is not on
//! the row and so arrives from
//! [`pending_count`](crate::models::follow::pending_count) — and the
//! top-level `also_known_as` alias list the account manages through
//! `/api/pleroma/aliases` (see
//! [`crate::api::pleroma_account_settings`]). Pleroma gates the `source`
//! block on the viewer being the account itself
//! (`AccountView.maybe_put_settings`), which is why no other route on this
//! module carries it.
//!
//! Authentication on `show` and `lookup` is optional, as in Pleroma's
//! `OAuthScopesPlug … fallback: :proceed_unauthenticated`: the routes
//! answer without a token. A token is not ignored, though: it names the
//! [`AccountViewer`] the entity is
//! rendered for, so the account's own user sees what the row hides from
//! strangers (hidden follow counts, hidden role flags, and the owner-only
//! keys of the `pleroma` block) while anyone else sees the restricted view.
//! An authenticated viewer looking at *another* account also gets their
//! [`Relationship`] embedded in `pleroma.relationship`, preloaded per
//! request; anonymous viewers and self-views keep Pleroma's empty object.
//! The [`MaybeAuthUser`] extractor is attached to both routes, so a request
//! presenting an invalid, expired or deactivated token is answered 401 "The
//! access token is invalid" instead of being silently treated as anonymous.
//!
//! `followers` and `following` are the account's relationship listings,
//! Pleroma's `.followers` and `.following`: [`MaybeAuthUser`] with the
//! `read:accounts` scope enforced on a presented user token
//! ([`require_user_scope`](MaybeAuthUser::require_user_scope)) and
//! [`PageParams`] keyset pagination (default limit 20, at most
//! [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)). They answer [`Account`]
//! entities — the accounts of the *accepted* follows, newest follow first,
//! local and remote alike — and paginate on the `follows` row: the
//! [`Link`](crate::pagination::link_header) header's `max_id`/`min_id`
//! carry [`Follow::id`](crate::models::follow::Follow::id), which is why
//! [`followers_page`](crate::models::follow::followers_page) returns
//! [`FollowPageEntry`](crate::models::follow::FollowPageEntry) rather than
//! bare rows. An authenticated viewer also gets their relationship to
//! every listed account embedded, one batched
//! [`Relationship::load_many`] per request. Privacy per Pleroma's
//! `cond`: an account with `hide_followers` (resp. `hide_follows`) answers
//! the empty array to everyone but itself — the count-hiding flags alone
//! never hide the list — while a locked account's lists stay visible,
//! pending requests excluded, to any viewer.
//!
//! `lookup` resolves its `acct` parameter through
//! [`get_by_acct`](crate::models::user::get_by_acct): a stored nickname,
//! case-insensitively, with this instance's own domain stripped for local
//! accounts. Resolving a handle this instance has never seen is left to
//! the `WebFinger` federation ticket.
//!
//! `relationships` requires auth: [`AuthUser`] plus the `read:follows`
//! scope, exactly Pleroma's plug pipeline for that action
//! (`%{scopes: ["read:follows"]}`). It reads Mastodon's `id[]` array
//! parameter — a single `id=` is accepted too, Pleroma's `List.wrap(id)` —
//! and answers one [`Relationship`] per *known* id in the order requested:
//! values that are not `UUID` text and ids that name no stored user are
//! skipped (as Pleroma's `User.get_all_by_ids` skips them), and an absent,
//! empty or wholly-unknown `id` is the empty array, never an error
//! (Pleroma returns `[]` when the parameter is missing). The flags come
//! from [`Relationship::load_many`], one batched query set per request.

use std::collections::HashSet;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Uri},
    routing::get,
};
use serde::Deserialize;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::{AuthUser, MaybeAuthUser},
    entities::{
        account::{Account, AccountViewer},
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    models::{
        follow,
        user::{self, User},
    },
    oauth::scopes::{READ_ACCOUNTS, READ_FOLLOWS},
    pagination::{PageParams, link_header, set_link_header},
    state::AppState,
    util::id::parse_id,
};

/// The 400 body message when the `acct` query parameter is absent or blank
/// (Mastodon's `ParameterMissing` wording).
const MISSING_ACCT: &str = "Missing parameter: acct";

/// The `Link` header's query parameters carry nothing but the cursor: the
/// relationship listings have no filter to repeat on either link (the
/// scrobbles listing's constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1/accounts` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/accounts/verify_credentials", get(verify_credentials))
        .route("/accounts/relationships", get(relationships))
        .route("/accounts/lookup", get(lookup_account))
        .route("/accounts/{id}/followers", get(list_followers))
        .route("/accounts/{id}/following", get(list_following))
        .route("/accounts/{id}", get(show_account))
}

/// `GET /api/v1/accounts/verify_credentials` — the caller's own account,
/// rendered as they see it: the owner view of [`Account::from_user_for`]
/// (hidden counters visible, the owner-only `pleroma` keys present) with
/// the `source` block [`Account::for_owner`] attaches.
///
/// The only thing that block needs beyond the token's row is the pending
/// follow-request queue, counted by
/// [`pending_count`](crate::models::follow::pending_count).
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token is app-only or does not
/// grant `read:accounts`. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn verify_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Account>> {
    auth.require(READ_ACCOUNTS)?;
    let owner = &auth.0;
    let requests = follow::pending_count(&state.db, owner.id).await?;
    let account = Account::for_owner(owner, &state.config, requests);
    Ok(Json(with_moved(&state, account, owner).await?))
}

/// Resolves [`moved_to_ap_id`](User::moved_to_ap_id) for one rendered
/// [`Account`]: when the row names a move target this instance has a row
/// for, [`Account::with_moved`] nests the target's plain entity. A move
/// to an actor id not mirrored here keeps `moved: null` (the alias and
/// move flows are what create that row; federation of a remote `Move`
/// arrives with a later ticket).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn with_moved(state: &AppState, account: Account, user: &User) -> AppResult<Account> {
    let Some(ap_id) = user.moved_to_ap_id.as_deref() else {
        return Ok(account);
    };
    Ok(match user::get_by_ap_id(&state.db, ap_id).await? {
        Some(target) => account.with_moved(&target, &state.config),
        None => account,
    })
}

/// `GET /api/v1/accounts/{id}` — the account whose id is `id`, local or
/// remote. An id that is not `UUID` text is a 404, not a 400, like every
/// other miss (see [`account_or_404`]).
///
/// The viewer behind an optional token is the account the entity is
/// rendered for; without one the anonymous view is answered. A viewer who
/// is looking at someone else gets their relationship to the account
/// embedded (see [`render_account`]).
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown or deactivated id;
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn show_account(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Account>> {
    let found = account_or_404(&state, &id).await?;
    Ok(Json(render_account(&state, viewer.as_ref(), &found).await?))
}

/// The query parameters of `GET /api/v1/accounts/lookup`. The absent and
/// blank cases are the documented 400 and 404, which the type cannot
/// express, so the field is optional here (the `webfinger` handler's
/// pattern).
#[derive(Debug, Deserialize)]
struct LookupQuery {
    /// The handle to resolve: `nick` or `nick@domain`.
    acct: Option<String>,
}

/// `GET /api/v1/accounts/lookup?acct=…` — the account a handle names,
/// resolved by [`get_by_acct`](crate::models::user::get_by_acct). The
/// viewer behind an optional token is the account the entity is rendered
/// for, as in `show`.
///
/// # Errors
///
/// [`AppError::BadRequest`] when `acct` is absent or blank;
/// [`AppError::NotFound`] when it names no live account (unknown, on a
/// foreign domain not mirrored here, or deactivated);
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn lookup_account(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    Query(query): Query<LookupQuery>,
) -> AppResult<Json<Account>> {
    let Some(acct) = query
        .acct
        .as_deref()
        .map(str::trim)
        .filter(|acct| !acct.is_empty())
    else {
        return Err(AppError::BadRequest(MISSING_ACCT.to_owned()));
    };
    let found = user::get_by_acct(&state.db, &state.config, acct)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)?;
    Ok(Json(render_account(&state, viewer.as_ref(), &found).await?))
}

/// `GET /api/v1/accounts/relationships?id[]=a&id[]=b` — the caller's
/// relationship to each requested account, as
/// [`Relationship::load_many`] computes it, in the order requested.
///
/// The `id` values are read by [`requested_ids`]; ones that are not `UUID`
/// text and ones that name no stored user are dropped (Pleroma's
/// `User.get_all_by_ids` simply finds nothing for them), so the response
/// holds only the known ids in requested order. No usable `id` at all is
/// the empty array, Pleroma's explicit `json(conn, [])` for a missing
/// parameter, not a `Missing parameter` error.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:follows`. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn relationships(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Relationship>>> {
    auth.require(READ_FOLLOWS)?;
    let requested = requested_ids(uri.query());
    if requested.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let targets = known_ids(&state, &requested).await?;
    Ok(Json(
        Relationship::load_many(&state.db, auth.0.id, &targets).await?,
    ))
}

/// The `id` parameters of a raw query string, as parsed values in
/// occurrence order: both the bracketed array form Mastodon clients send
/// (`id[]=a&id[]=b`, percent-encodings included) and a single `id=a`
/// (Pleroma's `List.wrap(id)`), or any mix of the two. Values that are not
/// `UUID` text are dropped here, already skipped.
fn requested_ids(query: Option<&str>) -> Vec<Uuid> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _value)| key == "id" || key == "id[]")
        .filter_map(|(_key, value)| parse_id(&value))
        .collect()
}

/// The subset of `requested` that names a stored user, kept in the
/// requested order ([`user::get_many`] answers in id order, so membership
/// is decided from its rows). One query per request whatever the batch
/// size.
async fn known_ids(state: &AppState, requested: &[Uuid]) -> AppResult<Vec<Uuid>> {
    let known: HashSet<Uuid> = user::get_many(&state.db, requested)
        .await?
        .into_iter()
        .map(|found| found.id)
        .collect();
    Ok(requested
        .iter()
        .copied()
        .filter(|id| known.contains(id))
        .collect())
}

/// `GET /api/v1/accounts/{id}/followers` — the accounts that follow `id`
/// with an accepted relationship, newest follow first, one [`PageParams`]
/// page at a time (limit 20 by default, at most
/// [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)). Auth is optional; a
/// presented user token must grant `read:accounts`, as in Pleroma's plug
/// for this action, and the viewer it names decides whose [`Relationship`]
/// the entries embed.
///
/// Pleroma's `followers` `cond`: an account that hides its followers is
/// answered the empty array — 200, no `Link` header — unless the viewer is
/// that account itself; `hide_followers_count` alone does not hide the
/// list, only the counter the entities carry. A locked account's follower
/// list is listed as any other's, its unanswered requests left out by the
/// model's `accepted` filter.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the authenticated token lacks `read:accounts`;
/// [`AppError::Unauthorized`] from the extractor for an invalid token;
/// [`AppError::NotFound`] for a malformed, unknown or deactivated account
/// id; [`AppError::BadRequest`] from [`PageParams`] for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_followers(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    if target.hide_followers.get() && !viewer_is(viewer, &target) {
        return Ok(empty_page());
    }
    let entries = follow::followers_page(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// `GET /api/v1/accounts/{id}/following` — the accounts `id` follows with
/// an accepted relationship, newest follow first. The exact counterpart
/// of [`list_followers`]: the same optional auth and `read:accounts`
/// gate, the same [`PageParams`] keyset pagination on the `follows` row,
/// and Pleroma's `following` `cond` hiding the list behind
/// [`hide_follows`](User::hide_follows) from everyone but the account
/// itself.
///
/// # Errors
///
/// Exactly those of [`list_followers`].
async fn list_following(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    if target.hide_follows.get() && !viewer_is(viewer, &target) {
        return Ok(empty_page());
    }
    let entries = follow::following_page(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// Renders `found` for `viewer` (who may be `None`), preloading the
/// viewer's relationship to the account when there is one to embed: a
/// viewer who is not the account itself, answered by a single-target
/// [`Relationship::load_many`]. Anyone else — anonymous, self — renders
/// with no preload, which keeps `pleroma.relationship` the empty object.
/// A stored move target is attached as the nested `moved` entity (see
/// [`with_moved`]).
async fn render_account(
    state: &AppState,
    viewer: AccountViewer<'_>,
    found: &User,
) -> AppResult<Account> {
    let relationships = preload_relationship(state, viewer, found).await?;
    let account = Account::from_user_for(viewer, found, &state.config, relationships.as_ref());
    with_moved(state, account, found).await
}

/// The single-account [`RelationshipMap`] for the `show`/`lookup` view:
/// [`Some`] only for a viewer looking at someone else, `None` when there
/// is nothing to embed (and so nothing to query).
async fn preload_relationship(
    state: &AppState,
    viewer: AccountViewer<'_>,
    found: &User,
) -> AppResult<Option<RelationshipMap>> {
    let Some(who) = viewer.filter(|who| who.id != found.id) else {
        return Ok(None);
    };
    let loaded =
        Relationship::load_many(&state.db, who.id, std::slice::from_ref(&found.id)).await?;
    let relationship = loaded.into_iter().next();
    Ok(relationship.map(|rel| RelationshipMap::from([(found.id, rel)])))
}

/// Whether `viewer` is looking at the account `target` itself — Pleroma's
/// `user.id == for_user.id` check in the `followers`/`following` `cond`,
/// by primary key. An anonymous request is never the target.
fn viewer_is(viewer: AccountViewer<'_>, target: &User) -> bool {
    viewer.is_some_and(|who| who.id == target.id)
}

/// The answer to a hidden relationship listing: the empty array as a plain
/// 200 with no `Link` header at all — Pleroma's `[]` branch, where the
/// page never got a cursor to link from.
fn empty_page() -> (HeaderMap, Json<Vec<Account>>) {
    (HeaderMap::new(), Json(Vec::new()))
}

/// Renders one fetched relationship page for `viewer`: every entry's
/// [`User`] as an [`Account`] with the viewer's relationship embedded
/// where one is preloaded (see [`preload_page_relationships`]), under the
/// `Link` header [`page_link`] derives from the relationship ids.
async fn page_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[follow::FollowPageEntry],
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let relationships = preload_page_relationships(state, viewer, entries).await?;
    let link = page_link(&state.config.base_url, path, entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = entries
        .iter()
        .map(|entry| {
            Account::from_user_for(viewer, &entry.user, &state.config, relationships.as_ref())
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// The [`RelationshipMap`] for one relationship page: the viewer's
/// relationship to every listed account in a single batched
/// [`Relationship::load_many`], keyed by target id (which is how
/// [`Account::from_user_for`] reads its preload). [`None`] only for an
/// anonymous viewer, whose entries keep Pleroma's empty
/// `pleroma.relationship` object and so need no query at all.
async fn preload_page_relationships(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[follow::FollowPageEntry],
) -> AppResult<Option<RelationshipMap>> {
    let Some(who) = viewer else {
        return Ok(None);
    };
    let targets: Vec<Uuid> = entries.iter().map(|entry| entry.user.id).collect();
    let loaded = Relationship::load_many(&state.db, who.id, &targets).await?;
    let map = loaded
        .into_iter()
        .zip(targets)
        .map(|(relationship, target)| (target, relationship))
        .collect();
    Ok(Some(map))
}

/// The `Link` header value for a fetched relationship page: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`follow::FollowPageEntry::follow_id`] — the cursors key on the
/// `follows` row, not on the users — and `None` for an empty page.
fn page_link(base_url: &str, path: &str, entries: &[follow::FollowPageEntry]) -> Option<String> {
    let newest = entries.first()?.follow_id.to_string();
    let oldest = entries.last()?.follow_id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of the ids the fixtures pass around.
    const ID_A: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// A second id, distinct from [`ID_A`].
    const ID_B: &str = "01934567-89ab-7cde-f012-3456789abcdf";

    /// The ids [`requested_ids`] found in `query`, as strings, so the
    /// assertions read like the query they came from.
    fn parsed(query: Option<&str>) -> Vec<String> {
        requested_ids(query)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn array_params_are_read_in_order() {
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "Mastodon's repeated bracketed params, in the order sent"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&other=x&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "unrelated params in between are ignored"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_A}"))),
            vec![ID_A, ID_A],
            "a repeated id is kept twice: the response answers what was asked"
        );
    }

    #[test]
    fn a_single_id_param_is_accepted() {
        assert_eq!(
            parsed(Some(&format!("id={ID_B}"))),
            vec![ID_B],
            "Pleroma's List.wrap accepts the single form"
        );
        assert_eq!(
            parsed(Some(&format!("id={ID_B}&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "and the two forms merge in occurrence order"
        );
    }

    #[test]
    fn values_are_percent_decoded_and_garbage_is_skipped() {
        assert_eq!(
            parsed(Some(&format!("id%5B%5D={ID_A}"))),
            vec![ID_A],
            "the bracketed key may arrive percent-encoded"
        );
        assert_eq!(
            parsed(Some("id[]=not-an-id")),
            Vec::<String>::new(),
            "a value that is not id text is skipped, not an error"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_B}&id=oops&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "one bad value does not sink its neighbours"
        );
    }

    #[test]
    fn nothing_usable_is_the_empty_list() {
        for query in [None, Some(""), Some("id="), Some("other=1"), Some("id")] {
            assert_eq!(
                parsed(query),
                Vec::<String>::new(),
                "`{query:?}` carries no usable id"
            );
        }
    }
}
