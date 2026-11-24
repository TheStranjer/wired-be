//! The relationship listings — `GET /api/v1/accounts/{id}/followers`
//! and `…/{id}/following` — with the page render they answer through
//! ([`page_response`], [`keyset_link`]) and the `RESTRICT_UNAUTHENTICATED`
//! profile gate the status listing shares ([`require_profile_read`]).
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

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use uuid::Uuid;

use crate::{
    api::helpers::visible_account_or_404,
    auth::MaybeAuthUser,
    config::Config,
    entities::{
        account::{Account, AccountViewer},
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    models::{follow, user::User},
    oauth::scopes::READ_ACCOUNTS,
    pagination::{NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    state::AppState,
};

/// The 403 body the account reads answer an anonymous request while the instance keeps the account's profile area restricted
/// (Pleroma's `EnsureAuthenticatedPlug` wording, the one [`crate::auth`]
/// renders for an app-only token).
const USER_REQUIRED: &str = "This API requires an authenticated user";

/// The `RESTRICT_UNAUTHENTICATED` area of this instance's own accounts'
/// profile reads (Pleroma's `profiles: %{local: …}`).
const PROFILES_LOCAL: &str = "profiles_local";

/// The [`PROFILES_LOCAL`] twin for a mirrored account of another instance
/// (Pleroma's `profiles: %{remote: …}`).
const PROFILES_REMOTE: &str = "profiles_remote";

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
/// id; [`AppError::Forbidden`] ("This API requires an authenticated user")
/// from [`require_profile_read`] for an anonymous request while
/// `RESTRICT_UNAUTHENTICATED` closes the account's profile half;
/// [`AppError::BadRequest`] from [`PageParams`] for a malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
pub(super) async fn list_followers(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = visible_account_or_404(&state, &id, auth.0.as_ref()).await?;
    let viewer = auth.0.as_ref();
    require_profile_read(&state.config, viewer, &target)?;
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
pub(super) async fn list_following(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = visible_account_or_404(&state, &id, auth.0.as_ref()).await?;
    let viewer = auth.0.as_ref();
    require_profile_read(&state.config, viewer, &target)?;
    if target.hide_follows.get() && !viewer_is(viewer, &target) {
        return Ok(empty_page());
    }
    let entries = follow::following_page(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// The `RESTRICT_UNAUTHENTICATED` gate of the account's reads (`show`,
/// `lookup`, `followers`, `following` and the status listing): an
/// authenticated request always passes — the restriction is about the
/// anonymous reader, as Pleroma's `visible_for` is — while an anonymous one
/// is the 403 [`USER_REQUIRED`] when this instance keeps the account's
/// profile area closed to it (see [`profile_area`] and
/// [`Config::restricts`]).
///
/// # Errors
///
/// [`AppError::Forbidden`] as described.
pub(super) fn require_profile_read(
    config: &Config,
    viewer: Option<&User>,
    target: &User,
) -> AppResult<()> {
    if viewer.is_some() || !config.restricts(profile_area(target)) {
        return Ok(());
    }
    Err(AppError::Forbidden(USER_REQUIRED.to_owned()))
}

/// The `RESTRICT_UNAUTHENTICATED` profile half `target` belongs to:
/// [`PROFILES_LOCAL`] when this instance authored the account,
/// [`PROFILES_REMOTE`] when it only mirrors it — decision 7's `local` column
/// is the whole test.
const fn profile_area(target: &User) -> &'static str {
    if target.local.get() {
        PROFILES_LOCAL
    } else {
        PROFILES_REMOTE
    }
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
/// `Link` header [`keyset_link`] derives from the relationship ids.
async fn page_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[follow::FollowPageEntry],
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let relationships = preload_page_relationships(state, viewer, entries).await?;
    // The cursors key on the `follows` row
    // ([`follow::FollowPageEntry::follow_id`]), not on the users.
    let link = keyset_link(
        &state.config.base_url,
        path,
        &NO_EXTRA_QUERY,
        entries,
        |e| e.follow_id,
    );
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

#[cfg(test)]
mod tests {
    use crate::api::accounts::test_support::{config_restricting, local_and_remote};

    use super::*;

    #[tokio::test]
    async fn the_profile_area_follows_the_local_column() {
        let (_db, alice, bob) = local_and_remote().await;
        assert_eq!(profile_area(&alice), PROFILES_LOCAL, "a local account");
        assert_eq!(
            profile_area(&bob),
            PROFILES_REMOTE,
            "a mirror is the other half"
        );
    }

    /// The `RESTRICT_UNAUTHENTICATED` gate: only an anonymous request to a
    /// restricted half is refused, and the two halves are independent.
    #[tokio::test]
    async fn restricted_areas_refuse_anonymous_reads_of_their_own_half() {
        let (_db, alice, bob) = local_and_remote().await;
        let open = Config::for_tests();
        assert!(
            require_profile_read(&open, None, &alice).is_ok(),
            "a public instance answers the anonymous request"
        );
        let locals = config_restricting(&[PROFILES_LOCAL]);
        let refused =
            require_profile_read(&locals, None, &alice).expect_err("a closed local profile");
        assert_eq!(
            refused.to_string(),
            USER_REQUIRED,
            "the refusal is Pleroma's authenticated-user wording"
        );
        assert!(
            require_profile_read(&locals, None, &bob).is_ok(),
            "`profiles_local` says nothing about a mirror"
        );
        let remotes = config_restricting(&[PROFILES_REMOTE]);
        assert!(
            require_profile_read(&remotes, None, &bob).is_err(),
            "the other half closes on its own name"
        );
        assert!(
            require_profile_read(&remotes, Some(&alice), &bob).is_ok(),
            "a presented user token is never refused for the restriction"
        );
        assert!(
            require_profile_read(&locals, Some(&bob), &alice).is_ok(),
            "whatever account it belongs to"
        );
    }
}
