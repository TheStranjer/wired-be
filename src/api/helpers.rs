//! Shared helpers for the API area handlers (`docs/ARCHITECTURE.md`
//! decision 3).
//!
//! Small request-level glue that several endpoints need and that belongs
//! neither inside one area's module nor in [`crate::util`] (which knows no
//! HTTP): resolving client-supplied ids to rows with exactly the error the
//! endpoint owes the client, for now, and more as later tickets need it.

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
