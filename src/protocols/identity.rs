//! Identifier schemes: how an account gets an ID on each protocol.
//!
//! A `WiredBE` account has one login (its nickname and password) and one
//! identity per protocol it is on. Each protocol has its own ID rules: a
//! future `ATProto` handle is a DNS name and cannot contain `_`, while
//! `WiredBE` nicknames can. So each protocol supplies, through
//! [`ProtocolAdapter`]:
//!
//! - a **validator** ([`validate_identifier`](ProtocolAdapter::validate_identifier)),
//!   which says yes or no to a requested ID;
//! - a **converter** ([`derive_identifier`](ProtocolAdapter::derive_identifier)),
//!   which turns any nickname into an ID that is definitely valid for that
//!   protocol.
//!
//! [`suggest_identifier`] is the shared helper that makes the converted ID
//! unique, using the protocol's
//! [`identifier_taken`](ProtocolAdapter::identifier_taken).
//!
//! **Each protocol's own main table is its identity record.** For
//! `ActivityPub` that is `activitypub_actors`: a row means the account is on
//! `ActivityPub`, and its `status` says [`Pending`](IdentityStatus::Pending)
//! or [`Active`](IdentityStatus::Active). There is no central identities
//! table; [`ProtocolAdapter::identity`] reads the protocol's own table and
//! answers `None` for an account that is not on the protocol.
//!
//! # Claiming
//!
//! [`ProtocolAdapter::claim`] writes the protocol's identity row for an
//! account, on the connection of the caller's transaction. The rules:
//!
//! - A claim is one step of a batch. Registration claims one identity per
//!   enabled protocol in the transaction that inserts the user; if any claim
//!   is invalid or [`Taken`](ClaimOutcome::Taken), the transaction rolls back
//!   and the response lists every failure under each protocol's
//!   [`identifier_field`](ProtocolAdapter::identifier_field). Enrolling an
//!   existing account later claims a single protocol with the same operation.
//! - The identifier is the one the registrant requested, validated by
//!   [`validate_identifier`](ProtocolAdapter::validate_identifier), or else
//!   the one [`derive_identifier`](ProtocolAdapter::derive_identifier) makes
//!   from the nickname. A derived ID that is taken is not made unique at
//!   registration (that is [`suggest_identifier`]'s job); the claim fails.
//! - A claim never checks first and then writes. It writes and relies on the
//!   protocol's unique constraint to answer [`Taken`](ClaimOutcome::Taken),
//!   so two racing claims cannot both win. A taken ID is an answer, not an
//!   error: an unusable connection or a failing statement is `Err`.
//! - A claim that answers [`Taken`](ClaimOutcome::Taken) leaves the
//!   transaction usable, so the caller can go on to collect the failures of
//!   the remaining protocols.

use sqlx::PgConnection;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    protocols::adapter::ProtocolAdapter,
};

/// How many candidates [`suggest_identifier`] tries: the derived ID, then `-2`, `-3` and so on.
const MAX_ATTEMPTS: u32 = 100;

/// Whether an account's identity on a protocol is claimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityStatus {
    /// The identity exists but is awaiting setup.
    Pending,
    /// The identity is claimed and in use.
    Active,
}

/// What claiming an identity on a protocol came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The identity row was written.
    Claimed,
    /// Another account holds the ID, or this account already has the identity.
    Taken,
}

/// An account's identity on one protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The ID in the protocol's own form.
    pub identifier: String,
    /// The ID as a URI, for linking identities across protocols.
    pub uri: String,
    /// Whether the identity is claimed.
    pub status: IdentityStatus,
}

/// Finds a free ID on `adapter`'s protocol for the account named `nickname`.
///
/// It derives the ID from the nickname, then while the ID is taken tries
/// `<derived>-2`, `<derived>-3` and so on. Every candidate is validated, and
/// one the protocol rejects (a suffix can push an ID past its length limit)
/// is skipped. Returns the validated, normalised ID.
///
/// # Errors
///
/// [`AppError::Conflict`] when none of the first 100 candidates is both
/// valid and free; [`AppError::Database`] when a lookup fails.
pub async fn suggest_identifier(
    adapter: &dyn ProtocolAdapter,
    conn: &mut PgConnection,
    nickname: &str,
    config: &Config,
) -> AppResult<String> {
    let derived = adapter.derive_identifier(nickname, config);
    for attempt in 1..=MAX_ATTEMPTS {
        let candidate = if attempt == 1 {
            derived.clone()
        } else {
            format!("{derived}-{attempt}")
        };
        let Ok(identifier) = adapter.validate_identifier(&candidate, config) else {
            continue;
        };
        if !adapter.identifier_taken(conn, &identifier).await? {
            return Ok(identifier);
        }
    }
    Err(AppError::Conflict(format!(
        "no free identifier found for {nickname}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::fixture::{Fixture, mark_taken};

    /// The derived ID is used as is while it is free.
    #[tokio::test]
    async fn free_derived_identifier_is_returned_unchanged() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut conn = db.pool().acquire().await.expect("a connection");

        let suggested =
            suggest_identifier(&Fixture, &mut conn, "Free_Spirit", &Config::for_tests()).await;

        assert_eq!(suggested.expect("an ID is found"), "free-spirit");
    }

    /// A taken derived ID gets the suffix `-2`.
    #[tokio::test]
    async fn taken_identifier_gets_suffix_two() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut conn = db.pool().acquire().await.expect("a connection");
        mark_taken("cool-guy");

        let suggested =
            suggest_identifier(&Fixture, &mut conn, "Cool_Guy", &Config::for_tests()).await;

        assert_eq!(suggested.expect("an ID is found"), "cool-guy-2");
    }

    /// The suffix keeps counting while candidates are taken.
    #[tokio::test]
    async fn suffix_counts_up_past_taken_candidates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut conn = db.pool().acquire().await.expect("a connection");
        mark_taken("busy-bee");
        mark_taken("busy-bee-2");
        mark_taken("busy-bee-3");

        let suggested =
            suggest_identifier(&Fixture, &mut conn, "busy_bee", &Config::for_tests()).await;

        assert_eq!(suggested.expect("an ID is found"), "busy-bee-4");
    }

    /// Past the attempt cap the helper fails instead of looping on.
    #[tokio::test]
    async fn exhausted_attempts_fail_with_a_conflict() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut conn = db.pool().acquire().await.expect("a connection");
        mark_taken("crowded");
        for attempt in 2..=MAX_ATTEMPTS {
            mark_taken(&format!("crowded-{attempt}"));
        }

        let suggested =
            suggest_identifier(&Fixture, &mut conn, "crowded", &Config::for_tests()).await;

        assert!(
            matches!(suggested, Err(AppError::Conflict(_))),
            "got {suggested:?}"
        );
    }

    /// A suffix that makes the ID invalid skips the candidate, and the search ends in an error.
    #[tokio::test]
    async fn candidates_the_protocol_rejects_are_skipped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut conn = db.pool().acquire().await.expect("a connection");
        let longest = "l".repeat(30);
        mark_taken(&longest);

        let suggested =
            suggest_identifier(&Fixture, &mut conn, &longest, &Config::for_tests()).await;

        assert!(
            matches!(suggested, Err(AppError::Conflict(_))),
            "every suffixed candidate is over 30 characters, got {suggested:?}"
        );
    }
}
