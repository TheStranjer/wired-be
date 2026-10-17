//! Shared helpers for the API area handlers (`docs/ARCHITECTURE.md`
//! decision 3).
//!
//! Small request-level glue that several endpoints need and that belongs
//! neither inside one area's module nor in [`crate::util`] (which knows no
//! HTTP): resolving client-supplied ids to rows with exactly the error the
//! endpoint owes the client, and reading the `id[]` array a batch endpoint
//! is keyed on, and more as later tickets need it.

use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{
        status::{self, Status},
        status_visibility,
        user::{self, User},
    },
    state::AppState,
    util::id::parse_id,
};

/// The keys the accounts batch endpoints read their ids from: Mastodon's
/// bracketed array plus the single value Pleroma's `List.wrap` accepts.
const ID_KEYS: [&str; 2] = ["id", "id[]"];
/// The deprecated alias of [`ID_KEYS`] that only the status batch lookup
/// answers to (`GET /api/v1/statuses`), per
/// [`requested_status_ids`].
const IDS_KEYS: [&str; 2] = ["ids", "ids[]"];

/// Resolves the `{id}` segment of an account route to its [`User`] row.
///
/// `id_str` must be `UUID` text as clients know it (decision 4: the
/// hyphenated or simple spelling, [`parse_id`]) and must name a row that
/// is still active. Anything else — a malformed id, an unknown id, a
/// deactivated account — is [`AppError::NotFound`]: Pleroma's
/// `assign_account_by_id` plus `visible_for` answer, and one that keeps
/// the endpoint from telling a bad id apart from a missing one. The show
/// endpoint (this ticket) and later routes keyed by account id share it.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
pub async fn account_or_404(state: &AppState, id_str: &str) -> AppResult<User> {
    let Some(id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    user::get_by_id(&state.db, id)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)
}

/// The `id` parameters of a raw query string, as parsed values in
/// occurrence order.
///
/// Both the bracketed array form Mastodon clients send (`id[]=a&id[]=b`,
/// percent-encodings included) and a single `id=a` (Pleroma's
/// `List.wrap(id)`), or any mix of the two. Values that are not `UUID` text
/// are dropped here, already skipped; whether an id names a stored account
/// is the caller's lookup (Pleroma's `User.get_all_by_ids` simply finds
/// nothing for an unknown one).
///
/// The batch endpoints keyed on a client-supplied id array share it: the
/// relationship batch (`GET /api/v1/accounts/relationships`) and the
/// familiar-followers batch (`GET /api/v1/accounts/familiar_followers`).
#[must_use]
pub fn requested_ids(query: Option<&str>) -> Vec<Uuid> {
    ids_under(query, &ID_KEYS)
}

/// The id array of `GET /api/v1/statuses`, which answers to both spellings.
///
/// [`requested_ids`]' `id[]` (and lone `id=`) and the deprecated `ids[]`
/// (and `ids=`) alias Pleroma's `StatusController.index` falls back to —
/// `Map.get(params, :id, Map.get(params, :ids))`. The `id` form wins
/// outright: a request that carries a usable `id` is answered from it and
/// its `ids` values are never read, exactly as that `Map.get` default
/// works. With no `id` at all the `ids` values are used, in occurrence
/// order; values that are not `UUID` text are dropped, already skipped.
///
/// How many of the ids an endpoint acts on is the caller's cap
/// ([`MAX_STATUS_IDS`](crate::api::statuses::MAX_STATUS_IDS)); this reads
/// them all.
#[must_use]
pub fn requested_status_ids(query: Option<&str>) -> Vec<Uuid> {
    let requested = ids_under(query, &ID_KEYS);
    if requested.is_empty() {
        return ids_under(query, &IDS_KEYS);
    }
    requested
}

/// The parsed values of the query params named by `keys`, in occurrence
/// order, with the values that are not `UUID` text dropped.
fn ids_under(query: Option<&str>, keys: &[&str]) -> Vec<Uuid> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _value)| keys.contains(&key.as_ref()))
        .filter_map(|(_key, value)| parse_id(&value))
        .collect()
}

/// Resolves the `{id}` segment of a status route to its live [`Status`]
/// row, as `viewer` may see it (`None`: an anonymous request).
///
/// `id_str` must be `UUID` text as clients know it (decision 4: the
/// hyphenated or simple spelling, [`parse_id`]). Everything that fails to
/// produce a row the viewer may read is the same [`AppError::NotFound`] —
/// a malformed id, an id that names no row, a tombstone (deleted), and a
/// status [`status_visibility`] hides from this viewer — so the endpoint
/// never reveals that a private post exists, which is Pleroma's
/// `StatusController.show` `with` block answering `{:error, :not_found}`
/// for both the missing and the invisible activity.
///
/// The row is the answer, not a rendered entity: this ticket's show route
/// renders it through [`crate::services::render_status`], and every later
/// `/api/v1/statuses/{id}/...` route (the context, history, source and
/// per-status state listings still to come) resolves and authorizes the
/// status through here and then reads what it needs from the row. A reblog
/// row is returned as itself; requesting a reblog's id yields the reblog,
/// whose original the renderer nests.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
pub async fn status_for_viewer_or_404(
    state: &AppState,
    viewer: Option<&User>,
    id_str: &str,
) -> AppResult<Status> {
    let Some(id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = status::get_by_id(&state.db, id).await? else {
        return Err(AppError::NotFound);
    };
    // Rule 7 of the visibility model reads the original of a reblog with
    // the same tests, so a reblog of a status this viewer may not see
    // fails here and is answered the same 404 as the original would be.
    if status_visibility::can_view(&state.db, &row, viewer).await? {
        Ok(row)
    } else {
        Err(AppError::NotFound)
    }
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

    /// The ids [`requested_status_ids`] found in `query`, as strings.
    fn status_parsed(query: Option<&str>) -> Vec<String> {
        requested_status_ids(query)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn the_status_batch_also_reads_the_ids_alias() {
        assert_eq!(
            status_parsed(Some(&format!("ids[]={ID_A}&ids[]={ID_B}"))),
            vec![ID_A, ID_B],
            "Pleroma's deprecated spelling arrives in occurrence order too"
        );
        assert_eq!(
            status_parsed(Some(&format!("ids={ID_B}"))),
            vec![ID_B],
            "and the alias has its own unwrapped form"
        );
        assert_eq!(
            status_parsed(Some(&format!("id[]={ID_A}&ids%5B%5D={ID_B}"))),
            vec![ID_A],
            "`id` wins outright, so the `ids` value alongside it is never read"
        );
    }

    #[test]
    fn the_status_batch_falls_back_only_when_id_carries_nothing() {
        assert_eq!(
            status_parsed(Some(&format!("id[]={ID_A}&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "the documented spelling is read as `requested_ids` reads it"
        );
        assert_eq!(
            status_parsed(Some("id[]=not-an-id&ids[]=also-not")),
            Vec::<String>::new(),
            "nothing usable under either spelling is the empty list"
        );
        assert_eq!(
            status_parsed(None),
            Vec::<String>::new(),
            "no query at all is the empty list, not an error"
        );
    }
}
