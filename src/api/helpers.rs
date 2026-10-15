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
    models::user::{self, User},
    state::AppState,
    util::id::parse_id,
};

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
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _value)| key == "id" || key == "id[]")
        .filter_map(|(_key, value)| parse_id(&value))
        .collect()
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
