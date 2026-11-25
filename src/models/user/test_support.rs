//! The fixtures the `user` module's unit tests share: the secret literals
//! the redaction checks hide, the local-account constructor (shared through
//! [`crate::test_fixtures`]), the read that
//! insists a row exists, and the little assertions more than one child's
//! tests call. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use super::{User, get_by_id};

pub(super) use crate::test_fixtures::local_new;

/// The instance origin the fixtures pretend to serve.
pub(super) const BASE: &str = "https://test.example";
/// A password hash that must never surface in `Debug` output.
pub(super) const SECRET_HASH: &str = "$argon2id$v=19$secret-digest";
/// A TOTP secret (inside `mfa_settings`) that must never surface in
/// `Debug` output.
pub(super) const SECRET_TOTP: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
/// An email-confirmation token: a link credential, so it must never
/// surface in `Debug` output either.
pub(super) const SECRET_CONFIRMATION: &str = "confirmation-token-c1JCzoTmgLmq9B1z7";

/// Reads the row behind `id` back out of the table, panicking if absent.
pub(super) async fn fetch(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id).await.unwrap().expect("row exists")
}

/// The ids of `users`, in the order the query answered them.
pub(super) fn ids_of(users: &[User]) -> Vec<Uuid> {
    users.iter().map(|found| found.id).collect()
}
