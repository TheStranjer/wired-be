//! The one answer of every account-list page: the blocked, muted and
//! endorsed accounts and the followers/following listings.
//!
//! Each of those endpoints fetches a newest-first page of rows that pair an
//! account with the row the page is keyed on (a `blocks`, `mutes`,
//! `endorsements` or `follows` row), and answers [`Account`] entities under a
//! `Link` header over that row's id. [`account_page`] is that render, stated
//! once; the endpoints differ only in how a row yields its cursor, its
//! [`User`] and any per-entry decoration, which is what [`AccountPageEntry`]
//! states for each row type.

use axum::{Json, http::HeaderMap};
use uuid::Uuid;

use crate::{
    entities::{
        account::{Account, AccountViewer},
        relationship::Relationship,
    },
    error::AppResult,
    models::user::User,
    pagination::{NO_EXTRA_QUERY, keyset_link, set_link_header},
    services::account_emojis::AccountEmojis,
    state::AppState,
};

/// One row of an account-list page.
///
/// A row is keyed on its own id (the cursor), carries the [`User`] the page
/// lists, and may decorate the rendered [`Account`].
pub trait AccountPageEntry {
    /// The id the page's `Link` cursors key on: the id of the row the page
    /// is paginated over, which is not the listed account's own id.
    fn cursor(&self) -> Uuid;

    /// The account this row lists.
    fn user(&self) -> &User;

    /// The rendered `account` with whatever this row adds to it; the
    /// account unchanged unless a listing says otherwise.
    fn decorate(&self, account: Account) -> Account {
        account
    }
}

/// Renders one fetched account-list page for `viewer`.
///
/// Every entry's [`User`] becomes an [`Account`] with the viewer's
/// relationship embedded where one is preloaded (one batched
/// [`Relationship::load_map`] over the page; an anonymous viewer costs no
/// query), then passes through [`AccountPageEntry::decorate`] — the mutes
/// listing attaches the row's `mute_expires_at` there. The `Link` header is
/// [`keyset_link`] over [`AccountPageEntry::cursor`] of the page's first and
/// last entry, repeating no query parameters beyond the cursor; an empty page
/// carries no `Link` header.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError::Database), and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`](crate::error::AppError::Internal).
pub async fn account_page<T: AccountPageEntry + Sync>(
    state: &AppState,
    viewer: AccountViewer<'_>,
    path: &str,
    entries: &[T],
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let targets: Vec<Uuid> = entries.iter().map(|entry| entry.user().id).collect();
    let relationships = Relationship::load_map(&state.db, viewer, &targets).await?;
    let link = keyset_link(
        &state.config.base_url,
        path,
        &NO_EXTRA_QUERY,
        entries,
        T::cursor,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let users: Vec<&User> = entries.iter().map(T::user).collect();
    let emojis = AccountEmojis::load(&state.db, &users).await?;
    let accounts = entries
        .iter()
        .map(|entry| {
            let account =
                Account::from_user_for(viewer, entry.user(), &state.config, relationships.as_ref());
            entry.decorate(emojis.fill(account, entry.user(), &state.config))
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// The answer to a hidden account listing: the empty array, no `Link`.
///
/// A plain 200 with no `Link` header at all — the branch where the page never
/// got a cursor to link from (a `hide_followers` / `hide_follows` account's
/// list asked by anyone but itself).
pub fn empty_account_page() -> (HeaderMap, Json<Vec<Account>>) {
    (HeaderMap::new(), Json(Vec::new()))
}

#[cfg(test)]
mod tests {
    use axum::http::header::LINK;

    use crate::config::Config;

    use super::*;

    /// A bare user as a page entry: keyed on its own id, undecorated.
    impl AccountPageEntry for User {
        fn cursor(&self) -> Uuid {
            self.id
        }

        fn user(&self) -> &User {
            self
        }
    }

    #[tokio::test]
    async fn an_empty_page_is_an_empty_array_without_a_link_header() {
        // A lazy pool: an anonymous viewer and no entries must reach no query.
        let state = AppState::lazy_for_tests(Config::for_tests());
        let entries: Vec<User> = Vec::new();
        let (headers, Json(accounts)) = account_page(&state, None, "/api/v1/blocks", &entries)
            .await
            .expect("an empty page renders");
        assert!(accounts.is_empty(), "no entries, no accounts");
        assert!(headers.get(LINK).is_none(), "an empty page has no Link");
    }

    #[test]
    fn the_hidden_listing_is_the_same_empty_page() {
        let (headers, Json(accounts)) = empty_account_page();
        assert!(accounts.is_empty(), "no accounts");
        assert!(headers.is_empty(), "no headers at all");
    }
}
