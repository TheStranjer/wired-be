//! The stored searches behind the three routes: whether statuses may be
//! searched at all, the status, hashtag and account arms, and the entity
//! renders each of them answers with.

use tracing::debug;

use crate::{
    entities::{account::Account, status::Status as StatusEntity},
    error::AppResult,
    models::{
        hashtag, status::Status as StatusRow, status_search, status_visibility, user::User,
        user_search,
    },
    services::{
        account_emojis::AccountEmojis,
        render_status::{ViewerMode, render_statuses},
    },
    state::AppState,
};

use super::{
    NO_OFFSET, Version,
    params::{HashtagList, SearchParams},
};
/// Whether a request may search statuses at all: every authenticated
/// viewer's may, an anonymous one only while the operator has turned
/// `SEARCH_STATUSES_UNAUTHENTICATED` on (decision 12's setting). The
/// visibility clause still decides *which* statuses each of them finds.
pub(super) fn statuses_are_searchable(state: &AppState, viewer: Option<&User>) -> bool {
    viewer.is_some() || state.config.search_statuses_unauthenticated.get()
}

/// The status entities the `statuses` arm answers with: the model's
/// visible, text-matched page — with the post the request resolved, when
/// the viewer may see it, riding first — rendered through the shared batch
/// renderer for the request's viewer (an anonymous render is Pleroma's
/// stranger view, exactly as on the timelines).
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn find_statuses(
    state: &AppState,
    viewer: Option<&User>,
    params: &SearchParams,
    resolved: Option<&StatusRow>,
) -> AppResult<Vec<StatusEntity>> {
    let mut rows = status_search::search_statuses(
        &state.db,
        viewer,
        &params.q,
        params.account_id,
        &params.text_page,
    )
    .await?;
    let mut page: Vec<StatusRow> = Vec::with_capacity(rows.len() + usize::from(resolved.is_some()));
    if let Some(top) = visible_resolution(state, viewer, resolved).await? {
        let id = top.id;
        rows.retain(|row| row.id != id);
        page.push(top);
    }
    page.append(&mut rows);
    let rendered = render_statuses(state, ViewerMode::User(viewer), &page).await?;
    debug!(
        count = rendered.len(),
        author_filtered = params.account_id.is_some(),
        resolved = page.len() > rows.len(),
        "statuses searched"
    );
    Ok(rendered)
}

/// The resolved status this request may answer with: [`can_view`] is the
/// shared gate, so a `private` or `direct` post the viewer has no business
/// reading is left out of the list exactly as the text search would never
/// have found it.
///
/// [`can_view`]: crate::models::status_visibility::can_view
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn visible_resolution(
    state: &AppState,
    viewer: Option<&User>,
    resolved: Option<&StatusRow>,
) -> AppResult<Option<StatusRow>> {
    let Some(row) = resolved else {
        return Ok(None);
    };
    if status_visibility::can_view(&state.db, row, viewer).await? {
        Ok(Some(row.clone()))
    } else {
        debug!("a resolved status the viewer may not see is left out of the answer");
        Ok(None)
    }
}

/// The hashtag arm: names stored with the query as their prefix, in the
/// answering version's spelling.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn find_hashtags(
    state: &AppState,
    params: &SearchParams,
    version: Version,
) -> AppResult<HashtagList> {
    let found = hashtag::search_prefix(
        &state.db,
        &params.q,
        params.text_page.limit,
        params.text_page.offset.unwrap_or(NO_OFFSET),
    )
    .await?;
    debug!(
        count = found.len(),
        version = ?version,
        "hashtags searched"
    );
    Ok(HashtagList::render(found, version, &state.config.base_url))
}

/// The account rows of one search answer, before any resolution and before
/// rendering: the model's ranked, viewer-filtered page for the query.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn account_rows(
    state: &AppState,
    viewer: Option<&User>,
    params: &SearchParams,
) -> AppResult<Vec<User>> {
    let rows = user_search::search_accounts(&state.db, viewer, &params.q, &params.opts()).await?;
    debug!(
        count = rows.len(),
        following = params.following,
        resolve = params.resolve,
        "accounts searched"
    );
    Ok(rows)
}

/// The account entities both search answers run through: `rows` rendered as
/// [`Account`] for the request's viewer (an anonymous request renders
/// Pleroma's stranger view; the listings here are not relationship
/// endpoints, so `pleroma.relationship` stays the empty object), with the
/// account the request resolved riding first — and left out of `rows` when
/// the fetch answered a row that is in the page after all, so one account is
/// never listed twice. Every entity carries the custom emojis its profile
/// uses, loaded once for the whole page.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn account_entities(
    state: &AppState,
    viewer: Option<&User>,
    rows: &[User],
    resolved: Option<&User>,
) -> AppResult<Vec<Account>> {
    let listed: Vec<&User> = rows
        .iter()
        .filter(|row| resolved.is_none_or(|found| row.id != found.id))
        .collect();
    let page: Vec<&User> = resolved.into_iter().chain(listed).collect();
    let emojis = AccountEmojis::load(&state.db, &page).await?;
    Ok(page
        .into_iter()
        .map(|found| {
            let account = Account::from_user_for(viewer, found, &state.config, None);
            emojis.fill(account, found, &state.config)
        })
        .collect())
}
