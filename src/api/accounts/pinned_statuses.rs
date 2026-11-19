//! The `pinned` branch of the account's status listing: the whole
//! featured set in pin order — newest pin first — uncursored and
//! without a `Link` header, answered by [`pinned_response`] and
//! ordered by [`ordered_by_pin`] over [`every_pin`]'s page.

use axum::{Json, http::HeaderMap};
use uuid::Uuid;

use crate::{
    entities::status::Status as StatusEntity,
    error::AppResult,
    models::{pinned_status, status::Status as StatusRow},
    pagination::PageParams,
    services::{
        render_status::{ViewerMode, render_statuses},
        timeline::TimelineQuery,
    },
    state::AppState,
};

/// The page `pinned` reads: every pin of the account, with no cursor and no
/// limit. The pagination keys are ignored because the set they would page
/// through is already bounded by [`max_pinned_statuses`] — the whole
/// profile's featured list is one page (see
/// [`list_for_user`](crate::models::pinned_status::list_for_user), the
/// model read this mirrors).
///
/// [`max_pinned_statuses`]: Config::max_pinned_statuses
pub(super) const fn every_pin() -> PageParams {
    PageParams {
        max_id: None,
        min_id: None,
        since_id: None,
        limit: i64::MAX,
        offset: None,
    }
}

/// The `pinned` answer: the account's pins in the order its profile
/// features them — newest pin first, no `Link` header, because there is no
/// older page of a list that never pages (Pleroma's `pinned` branch adds
/// its restriction to the same fetch, so the request's other filters still
/// apply).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn pinned_response(
    state: &AppState,
    query: &TimelineQuery,
    user_id: Uuid,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    let rows = query.fetch(&state.db).await?;
    let rows = ordered_by_pin(state, user_id, rows).await?;
    let statuses = render_statuses(state, ViewerMode::User(query.viewer.as_ref()), &rows).await?;
    Ok((HeaderMap::new(), Json(statuses)))
}

/// `rows` in the account's pin order:
/// [`pinned_ids`](crate::models::pinned_status::pinned_ids) reads each row's
/// pin time and the sort applies the order
/// [`crate::models::pinned_status`]'s `LIST_SQL` states — newest pin first,
/// the status id breaking a same-instant tie. A row with no pin (the query
/// cannot answer one, but the comparator does not assume it) sorts by that
/// same id order, so the sort stays total.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn ordered_by_pin(
    state: &AppState,
    user_id: Uuid,
    mut rows: Vec<StatusRow>,
) -> AppResult<Vec<StatusRow>> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let pins = pinned_status::pinned_ids(&state.db, user_id, &ids).await?;
    rows.sort_by(|a, b| match (pins.get(&a.id), pins.get(&b.id)) {
        (Some(a_at), Some(b_at)) if a_at != b_at => b_at.cmp(a_at),
        _ => b.id.cmp(&a.id),
    });
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pin_is_the_uncursored_whole_set() {
        let page = every_pin();
        assert_eq!(page.limit, i64::MAX, "no page of the featured list");
        assert_eq!(
            (page.max_id, page.min_id, page.since_id, page.offset),
            (None, None, None, None),
            "and no cursor either"
        );
    }
}
