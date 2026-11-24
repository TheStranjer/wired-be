//! Account aliases: listing, adding and dropping the `alsoKnownAs` entries a
//! move needs.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::{looks_like_ap_id, success_body};
use crate::{
    auth::AuthUser,
    entities::account::full_nickname,
    error::{AppError, AppResult},
    extract::ApiBody,
    federation::fetch_actor::{get_or_fetch_by_acct, get_or_fetch_by_ap_id},
    models::user::{self, User, get_by_ap_id},
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    services::federation_out,
    state::AppState,
};

/// The `{"aliases": […]}` body of [`list_aliases`].
#[derive(Debug, Serialize)]
pub(super) struct AliasesBody {
    /// The account's aliases as `acct` handles (or raw actor URLs when
    /// this instance has no row for the id).
    aliases: Vec<String>,
}

/// The parameters of `PUT`/`DELETE /api/pleroma/aliases`: the alias to
/// add or drop, as an `acct` handle or an `ActivityPub` actor id.
#[derive(Debug, Deserialize)]
pub(super) struct AliasParams {
    /// The alias the client names.
    alias: String,
}

/// `GET /api/pleroma/aliases` — the account's alias list as handles.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:accounts` check. Any database failure arrives through
/// `?` as [`AppError::Database`].
pub(super) async fn list_aliases(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<AliasesBody>> {
    auth.require(READ_ACCOUNTS)?;
    let aliases = alias_handles(&state, &auth.0).await?;
    Ok(Json(AliasesBody { aliases }))
}

/// The stored alias actor ids rendered one by one: the full `acct`
/// nickname of the mirrored actor when this instance has a row for the
/// id (Pleroma's `list_aliases` maps through `User.full_nickname/1`),
/// the raw URL when it has none.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn alias_handles(state: &AppState, account: &User) -> AppResult<Vec<String>> {
    let mut handles = Vec::with_capacity(account.also_known_as.len());
    for ap_id in &account.also_known_as {
        let handle = get_by_ap_id(&state.db, ap_id).await?.map_or_else(
            || ap_id.clone(),
            |found| full_nickname(&found, &state.config),
        );
        handles.push(handle);
    }
    Ok(handles)
}

/// `PUT /api/pleroma/aliases` — append an alias. The target is resolved
/// through the federation get-or-fetch (see [`resolve_alias`]); its
/// actor id joins the list unless it is already there, so adding twice
/// is one entry. Answers `{"status":"success"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] (the generic
/// "Record not found" body) when the alias resolves to no account. Any
/// database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn add_alias(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<AliasParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let target = resolve_alias(&state, &params.alias).await?;
    let mut aliases = auth.0.also_known_as.clone();
    if !aliases.contains(&target.ap_id) {
        aliases.push(target.ap_id.clone());
        user::set_also_known_as(&state.db, auth.0.id, &aliases).await?;
        announce_aliases(&state, auth.0.id).await?;
    }
    Ok(success_body())
}

/// Publishes the account's profile `Update` after its alias list changed,
/// so peers learn the new `alsoKnownAs` (a `Move` is refused unless the
/// target lists the origin). The row is re-read so the document carries
/// what was just stored.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn announce_aliases(state: &AppState, account_id: Uuid) -> AppResult<()> {
    if let Some(updated) = user::get_by_id(&state.db, account_id).await? {
        federation_out::after_profile_updated(state, &updated).await;
    }
    Ok(())
}

/// `DELETE /api/pleroma/aliases` — drop an alias. Both spellings a
/// client may echo back are removed: the string exactly as given (an
/// entry [`list_aliases`] rendered as its raw URL falls back to that)
/// and, when the input names a row this instance knows, that row's
/// actor id. A list without a matching entry is written unchanged; the
/// answer is `{"status":"success"}` either way.
///
/// Unlike `PUT` this never touches the network: removal needs no fresh
/// view of the target, only its stored id.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. Any database failure arrives through
/// `?` as [`AppError::Database`].
pub(super) async fn delete_alias(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<AliasParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let stored = &auth.0.also_known_as;
    let kept = dropped_aliases(&state, &params.alias, stored).await?;
    if kept.len() != stored.len() {
        user::set_also_known_as(&state.db, auth.0.id, &kept).await?;
        announce_aliases(&state, auth.0.id).await?;
    }
    Ok(success_body())
}

/// The alias list with `input` removed, as [`delete_alias`] writes it:
/// entries equal to the input verbatim, or to the actor id of the row
/// the input names (the mirrored-alias case, where `GET` showed a handle
/// rather than the raw id). The `Vec` is rebuilt (order-preserving) so a
/// client that added the same alias under both spellings is fully
/// cleared.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn dropped_aliases(
    state: &AppState,
    input: &str,
    stored: &[String],
) -> AppResult<Vec<String>> {
    let known_id = user::get_by_acct(&state.db, &state.config, input)
        .await?
        .map(|found| found.ap_id);
    Ok(without_alias(stored, input, known_id.as_deref()))
}

/// The list-editing half of [`dropped_aliases`], pure: keep every entry
/// that is neither the input verbatim nor the resolved actor id
/// (`known_id`, `None` when the input names no row).
fn without_alias(stored: &[String], input: &str, known_id: Option<&str>) -> Vec<String> {
    stored
        .iter()
        .filter(|alias| **alias != input && Some(alias.as_str()) != known_id)
        .cloned()
        .collect()
}

/// Resolves an alias string to the account behind it: an `http(s)` URL
/// goes through [`get_or_fetch_by_ap_id`], anything else through
/// [`get_or_fetch_by_acct`] (which answers from the stored nickname when
/// there is one, and only then reaches for `WebFinger`). A failure to
/// resolve is reported as the plain 404 the ticket fixes; the fetcher's
/// own reason is logged, never sent.
///
/// [`get_or_fetch_by_ap_id`]: crate::federation::fetch_actor::get_or_fetch_by_ap_id
/// [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
pub(super) async fn resolve_alias(state: &AppState, alias: &str) -> AppResult<User> {
    let fetched = if looks_like_ap_id(alias) {
        get_or_fetch_by_ap_id(state, alias).await
    } else {
        get_or_fetch_by_acct(state, alias).await
    };
    fetched.map_err(|err| {
        debug!(%err, alias, "alias target could not be resolved");
        AppError::NotFound
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{ALIAS, aliases};
    use super::*;

    /// The pure removal filter behind [`dropped_aliases`]: both spellings
    /// of the named alias go, everything else stays in order.
    #[test]
    fn removal_drops_the_verbatim_input_and_the_resolved_id() {
        let bob = "https://test.example/users/bob";
        let stored = aliases(&[bob, "https://x.example/users/y"]);

        let kept = without_alias(&stored, bob, Some(bob));
        assert_eq!(
            kept,
            vec![ALIAS.to_owned(), "https://x.example/users/y".to_owned()],
            "the verbatim spelling drops the entry with the same id"
        );

        let by_handle = without_alias(&stored, "bob@remote.example", Some(bob));
        assert_eq!(
            by_handle,
            vec![ALIAS.to_owned(), "https://x.example/users/y".to_owned()],
            "a handle spelling drops the row's actor id"
        );

        let untouched = without_alias(&stored, "ghost@nowhere", None);
        assert_eq!(
            untouched, stored,
            "an input nothing matches keeps every entry"
        );
    }
}
