//! The `invite_tokens` table: registration invites gated by
//! `INVITES_ENABLED` (see [`crate::config`]).
//!
//! Reference: Pleroma's `Pleroma.UserInviteToken`
//! (`lib/pleroma/user_invite_token.ex`) and its `user_invite_tokens`
//! table. An invite is a random token text (16 URL-safe-random bytes from
//! [`random_token`]) plus one of four lifecycle types, derived from the
//! limits [`InviteParams`] carries exactly like Pleroma's `assign_type`:
//!
//! | limits submitted              | [`InviteType`]              |
//! |-------------------------------|-----------------------------|
//! | neither                       | `OneTime`                   |
//! | `max_use`                     | `Reusable`                  |
//! | `expires_at`                  | `DateLimited`               |
//! | both                          | `ReusableDateLimited`       |
//!
//! [`InviteToken::is_valid`] answers whether an invite may still register
//! someone on a given day: it was never spent (`used` is false), its
//! expiry — when it carries one — is today or later, and its use count —
//! when capped — has not reached `max_use`. [`consume`] records one registration against the
//! invite: it raises `uses` and marks `used` for a `OneTime` invite or once
//! `max_use` is reached, so a `DateLimited` invite serves every registration
//! up to its date while a capped one stops exactly at its cap. The
//! registration endpoint consumes on the transaction that creates the user,
//! so an invite is never spent by a registration that did not happen.
//!
//! `token` is secret material until the operator hands the invite out: the
//! manual [`Debug`](InviteToken) impl below redacts it. [`create`],
//! [`list`] and [`revoke`] are the maintenance surface the later admin
//! tickets drive.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    util::{id::new_id, token::random_token},
};

/// Bytes behind an invite token: 16 random bytes, base64url-encoded to 22
/// URL-safe characters by [`random_token`] (Pleroma mints 32; the ticket
/// fixes 16 here).
const INVITE_TOKEN_BYTES: usize = 16;

/// [`create`]: the id this module mints (decision 4) plus the five columns
/// an invite carries from its parameters, with `uses`, `used` and
/// `inserted_at` left to the column defaults. `RETURNING *` brings the row
/// back with those stamps.
const INSERT_SQL: &str = "INSERT INTO invite_tokens (id, token, invite_type, expires_at, max_use) \
     VALUES ($1, $2, $3, $4, $5) RETURNING *";

/// [`get_by_token`]: the one lookup the registration gate runs — the exact,
/// case-sensitive token text is the unique key.
const SELECT_BY_TOKEN_SQL: &str = "SELECT * FROM invite_tokens WHERE token = $1";

/// [`list`]: every invite, oldest first on the id whose text sorts in
/// creation order (decision 4), the order Pleroma's `list_invites` keeps.
const LIST_SQL: &str = "SELECT * FROM invite_tokens ORDER BY id";

/// [`revoke`]: one invite by its token text.
const DELETE_BY_TOKEN_SQL: &str = "DELETE FROM invite_tokens WHERE token = $1";

/// [`consume`]: one registration recorded against the invite. The `uses`
/// and `used` on the right-hand side are the pre-update values, so one
/// statement raises the count and decides the spent flag from the same
/// snapshot; `used` stays sticky once set.
const CONSUME_SQL: &str = "UPDATE invite_tokens
       SET uses = uses + 1,
           used = used OR invite_type = 'one_time'
                    OR (max_use IS NOT NULL AND uses + 1 >= max_use)
     WHERE id = $1";

/// The lifecycle of an [`InviteToken`], stored in the `invite_type` text
/// column as its `snake_case` variant name (which is exactly what the
/// column's `CHECK` constraint allows).
///
/// Pleroma derives the type from the limits an invite was created with
/// ([`InviteType::for_params`]); it is never edited afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum InviteType {
    /// Serves exactly one registration: [`consume`] marks it `used`.
    OneTime,
    /// Serves up to `max_use` registrations: [`consume`] marks it `used`
    /// when that count is reached.
    Reusable,
    /// Serves every registration until its `expires_at` day has passed;
    /// never marked used by consumption.
    DateLimited,
    /// The combination: until `expires_at`, at most `max_use` times.
    ReusableDateLimited,
}

impl InviteType {
    /// The type [`create`] stores for an invite carrying `expires_at`
    /// and/or `max_use`: the derivation Pleroma's `assign_type` applies
    /// (both limits [`Self::ReusableDateLimited`], only the date
    /// [`Self::DateLimited`], only the cap [`Self::Reusable`], neither
    /// [`Self::OneTime`]).
    #[must_use]
    pub const fn for_params(expires_at: Option<NaiveDate>, max_use: Option<i32>) -> Self {
        match (max_use, expires_at) {
            (Some(_), Some(_)) => Self::ReusableDateLimited,
            (None, Some(_)) => Self::DateLimited,
            (Some(_), None) => Self::Reusable,
            (None, None) => Self::OneTime,
        }
    }
}

/// What [`create`] is handed to mint one invite: the two limits, from which
/// the [`InviteType`] follows. The token text is never an input — every
/// invite's secret is freshly random.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InviteParams {
    /// Last day the invite is valid; `None` never expires.
    pub expires_at: Option<NaiveDate>,
    /// Registrations the invite may serve in total; `None` is uncapped.
    pub max_use: Option<i32>,
}

impl InviteParams {
    /// The uncapped, never-expiring parameters of a one-time invite.
    #[must_use]
    pub const fn one_time() -> Self {
        Self {
            expires_at: None,
            max_use: None,
        }
    }
}

/// One row of `invite_tokens`: a registration invite.
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct InviteToken {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// The secret text a registrant submits as `token`; unique, and
    /// redacted from `Debug` output.
    pub token: String,
    /// Which limits this invite carries ([`InviteType::for_params`]).
    pub invite_type: InviteType,
    /// Last day this invite is valid; `None` never expires.
    pub expires_at: Option<NaiveDate>,
    /// Total registrations this invite may serve; `None` is uncapped.
    pub max_use: Option<i32>,
    /// Registrations served so far; raised by every [`consume`].
    pub uses: i32,
    /// Whether the invite is spent: set by [`consume`] for a
    /// [`InviteType::OneTime`] invite or once [`max_use`](Self::max_use)
    /// is reached.
    pub used: bool,
    /// When the invite was created.
    pub inserted_at: DateTime<Utc>,
}

/// [`token`](Self::token) is secret material and renders as `<redacted>`;
/// everything else shows.
impl fmt::Debug for InviteToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteToken")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("invite_type", &self.invite_type)
            .field("expires_at", &self.expires_at)
            .field("max_use", &self.max_use)
            .field("uses", &self.uses)
            .field("used", &self.used)
            .field("inserted_at", &self.inserted_at)
            .finish()
    }
}

impl InviteToken {
    /// Whether this invite may still register someone on `today` (the
    /// instance's calendar date): never spent ([`used`](Self::used) is
    /// false), its [`expires_at`](Self::expires_at) — when it carries one
    /// — is `today` or later, and its [`uses`](Self::uses) — under a
    /// [`max_use`](Self::max_use) cap — still below the cap. The check is
    /// the conjunction of the limits themselves, so it agrees with every
    /// [`InviteType`]'s rule: a `DateLimited` invite has no cap and no
    /// spent-by-consumption flag, a `Reusable` one has no date.
    #[must_use]
    pub fn is_valid(&self, today: NaiveDate) -> bool {
        !self.used
            && self.expires_at.is_none_or(|day| day >= today)
            && self.max_use.is_none_or(|cap| self.uses < cap)
    }
}

/// Mints an invite from `params`: a fresh [`random_token`] secret, the
/// [`InviteType`] its limits derive, and the `UUIDv7` key (decision 4);
/// returns the stored row with its zero counts and database stamp.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The random token
/// colliding with a stored one is a database failure too — vanishingly
/// unlikely for 16 CSPRNG bytes, and the operator just creates again.
pub async fn create(pool: &PgPool, params: &InviteParams) -> AppResult<InviteToken> {
    let invite_type = InviteType::for_params(params.expires_at, params.max_use);
    let row = sqlx::query_as::<_, InviteToken>(INSERT_SQL)
        .bind(new_id())
        .bind(random_token(INVITE_TOKEN_BYTES))
        .bind(invite_type)
        .bind(params.expires_at)
        .bind(params.max_use)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Fetches the invite whose token text is exactly `token`, `None` when no
/// invite carries it (or it was revoked).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_token(pool: &PgPool, token: &str) -> AppResult<Option<InviteToken>> {
    let row = sqlx::query_as::<_, InviteToken>(SELECT_BY_TOKEN_SQL)
        .bind(token)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Every invite, oldest first (creation order on the id, decision 4): the
/// maintenance listing the later admin tickets serve.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool) -> AppResult<Vec<InviteToken>> {
    let invites = sqlx::query_as::<_, InviteToken>(LIST_SQL)
        .fetch_all(pool)
        .await?;
    Ok(invites)
}

/// Deletes the invite whose token text is exactly `token`, reporting
/// whether there was one to remove. Revocation is a delete, as in
/// Pleroma's admin API: a spent-invite history is not kept here.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn revoke(pool: &PgPool, token: &str) -> AppResult<bool> {
    let outcome = sqlx::query(DELETE_BY_TOKEN_SQL)
        .bind(token)
        .execute(pool)
        .await?;
    Ok(outcome.rows_affected() > 0)
}

/// Records one registration against the invite `id`: raises `uses` and
/// marks the row `used` for a [`InviteType::OneTime`] invite or once
/// `max_use` is reached — the accounting of [`InviteToken::is_valid`]'s
/// limits.
///
/// Runs on the caller's open transaction (or any executor), so the
/// registration endpoint spends the invite in the same transaction that
/// creates the user: a rolled-back registration spends nothing.
///
/// # Errors
///
/// Any database failure (connection, constraint) arrives as
/// [`AppError::Database`](crate::error::AppError). Consuming an id no
/// invite carries (one revoked under the caller's feet) is reported as
/// [`AppError::Internal`]: the caller read the row moments ago on the
/// same connection, so a missed update is a broken expectation, not a
/// client error.
pub async fn consume<'e, E>(executor: E, id: Uuid) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    let outcome = sqlx::query(CONSUME_SQL).bind(id).execute(executor).await?;
    if outcome.rows_affected() == 0 {
        return Err(AppError::Internal(
            "invite consumption matched no row".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;

    use super::*;

    /// Today on the instance clock, the `is_valid` day argument.
    fn today() -> NaiveDate {
        Utc::now().date_naive()
    }

    /// `today` shifted by `days`.
    fn day_shift(days: i64) -> NaiveDate {
        today() + ChronoDuration::days(days)
    }

    /// An [`InviteToken`] row assembled by hand for the pure [`is_valid`]
    /// checks: only the four limit fields matter there.
    fn invite(
        invite_type: InviteType,
        expires_at: Option<NaiveDate>,
        max_use: Option<i32>,
        use_count: i32,
        spent: bool,
    ) -> InviteToken {
        InviteToken {
            id: Uuid::now_v7(),
            token: "secret-text".to_owned(),
            invite_type,
            expires_at,
            max_use,
            uses: use_count,
            used: spent,
            inserted_at: Utc::now(),
        }
    }

    /// The stored row behind `id`, re-read to prove what a write changed.
    async fn stored(pool: &PgPool, id: Uuid) -> InviteToken {
        let row = sqlx::query_as::<_, InviteToken>("SELECT * FROM invite_tokens WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .expect("the row reads back");
        row.expect("the invite exists")
    }

    /// Consumes on a fresh transaction that is rolled back, leaving the
    /// database untouched: the proof the write really runs on its executor.
    async fn consume_rolled_back(pool: &PgPool, id: Uuid) {
        let mut tx = pool.begin().await.expect("a transaction begins");
        consume(&mut *tx, id)
            .await
            .expect("consume runs on the transaction");
        tx.rollback().await.expect("the rollback runs");
    }

    #[test]
    fn limits_derive_the_type() {
        assert_eq!(
            InviteType::for_params(None, None),
            InviteType::OneTime,
            "no limits is the single-use invite"
        );
        assert_eq!(
            InviteType::for_params(None, Some(3)),
            InviteType::Reusable,
            "a cap alone is reusable"
        );
        assert_eq!(
            InviteType::for_params(Some(day_shift(1)), None),
            InviteType::DateLimited,
            "a date alone is date-limited"
        );
        assert_eq!(
            InviteType::for_params(Some(day_shift(1)), Some(3)),
            InviteType::ReusableDateLimited,
            "both limits combine"
        );
    }

    #[test]
    fn is_valid_weighs_the_three_limits_together() {
        let fresh = today();
        assert!(
            invite(InviteType::OneTime, None, None, 0, false).is_valid(fresh),
            "an unused, uncapped, undated invite is valid"
        );
        assert!(
            !invite(InviteType::OneTime, None, None, 0, true).is_valid(fresh),
            "a spent invite is invalid whatever else it carries"
        );
        assert!(
            invite(InviteType::DateLimited, Some(fresh), None, 0, false).is_valid(fresh),
            "the expiry day itself is still valid (today-or-later)"
        );
        assert!(
            !invite(
                InviteType::DateLimited,
                Some(fresh - ChronoDuration::days(1)),
                None,
                0,
                false
            )
            .is_valid(fresh),
            "the day after the expiry is not"
        );
        assert!(
            invite(InviteType::Reusable, None, Some(2), 1, false).is_valid(fresh),
            "one registration short of the cap is valid"
        );
        assert!(
            !invite(InviteType::Reusable, None, Some(2), 2, false).is_valid(fresh),
            "the cap reached is invalid"
        );
        assert!(
            !invite(InviteType::Reusable, None, Some(2), 5, false).is_valid(fresh),
            "and past it"
        );
        assert!(
            invite(
                InviteType::ReusableDateLimited,
                Some(fresh + ChronoDuration::days(3)),
                Some(1),
                0,
                false,
            )
            .is_valid(fresh),
            "a combination with every limit ahead"
        );
        assert!(
            invite(InviteType::DateLimited, Some(fresh), None, 9, false).is_valid(fresh),
            "a date-limited invite has no cap: its uses count never refuses it"
        );
    }

    #[test]
    fn invite_debug_redacts_the_token() {
        let debug = format!("{:?}", invite(InviteType::OneTime, None, None, 0, false));
        assert!(
            !debug.contains("secret-text"),
            "the token must not leak into Debug output"
        );
        assert!(
            debug.contains(r#"token: "<redacted>""#),
            "the redaction marker names the field"
        );
    }

    #[tokio::test]
    async fn create_mints_a_fresh_one_time_invite() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = create(&pool, &InviteParams::one_time())
            .await
            .expect("the insert runs");
        assert_ne!(first.id, Uuid::nil(), "the id is minted");
        assert_eq!(
            first.token.len(),
            22,
            "16 bytes are 22 base64url characters"
        );
        assert_eq!(
            first.invite_type,
            InviteType::OneTime,
            "no limits derive one_time"
        );
        assert_eq!(first.expires_at, None, "no expiry stored");
        assert_eq!(first.max_use, None, "no cap stored");
        assert_eq!(first.uses, 0, "nothing spent yet");
        assert!(!first.used, "not used yet");
        assert!(
            first
                .token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "the token is URL-safe verbatim"
        );
        let second = create(&pool, &InviteParams::one_time())
            .await
            .expect("the second insert runs");
        assert_ne!(first.token, second.token, "two mints do not collide");

        assert!(first.is_valid(today()), "a fresh invite is valid");
    }

    #[tokio::test]
    async fn create_stores_the_limits_and_derives_the_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let expiry = day_shift(30);
        for (params, want) in [
            (
                InviteParams {
                    expires_at: Some(expiry),
                    max_use: None,
                },
                InviteType::DateLimited,
            ),
            (
                InviteParams {
                    expires_at: None,
                    max_use: Some(5),
                },
                InviteType::Reusable,
            ),
            (
                InviteParams {
                    expires_at: Some(expiry),
                    max_use: Some(5),
                },
                InviteType::ReusableDateLimited,
            ),
        ] {
            let created = create(&pool, &params).await.expect("the insert runs");
            assert_eq!(created.invite_type, want, "{params:?} derives {want:?}");
            assert_eq!(created.expires_at, params.expires_at, "expiry round-trips");
            assert_eq!(created.max_use, params.max_use, "cap round-trips");
            let fetched = get_by_token(&pool, &created.token)
                .await
                .expect("the lookup runs")
                .expect("the exact token finds the row");
            assert_eq!(fetched, created, "get_by_token answers the stored row");
        }
        let miss = get_by_token(&pool, "never-minted-text")
            .await
            .expect("the lookup runs");
        assert_eq!(miss, None, "an unknown token finds no invite");
    }

    #[tokio::test]
    async fn consume_spends_a_one_time_invite() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let created = create(&pool, &InviteParams::one_time())
            .await
            .expect("the insert runs");
        consume(&pool, created.id)
            .await
            .expect("the first consume runs");
        let after = stored(&pool, created.id).await;
        assert_eq!(after.uses, 1, "the count rose");
        assert!(after.used, "one_time marks the invite used");
        assert!(!after.is_valid(today()), "and it never registers again");

        consume_rolled_back(&pool, after.id).await;
        let unchanged = stored(&pool, created.id).await;
        assert_eq!(unchanged.uses, 1, "a rolled-back transaction spent nothing");
    }

    #[tokio::test]
    async fn consume_counts_a_reusable_invite_up_to_its_cap() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let params = InviteParams {
            expires_at: None,
            max_use: Some(2),
        };
        let created = create(&pool, &params).await.expect("the insert runs");
        consume(&pool, created.id)
            .await
            .expect("the first consume runs");
        let once = stored(&pool, created.id).await;
        assert_eq!(once.uses, 1, "one of two served");
        assert!(!once.used, "one registration short of the cap stays open");
        assert!(once.is_valid(today()), "and it is still valid");
        consume(&pool, once.id)
            .await
            .expect("the second consume runs");
        let twice = stored(&pool, created.id).await;
        assert_eq!(twice.uses, 2, "both served");
        assert!(twice.used, "reaching max_use marks the invite used");
        assert!(!twice.is_valid(today()), "and it is exhausted");
    }

    #[tokio::test]
    async fn a_date_limited_invite_counts_uses_but_never_marks_used() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let params = InviteParams {
            expires_at: Some(day_shift(1)),
            max_use: None,
        };
        let created = create(&pool, &params).await.expect("the insert runs");
        consume(&pool, created.id)
            .await
            .expect("the first consume runs");
        consume(&pool, created.id)
            .await
            .expect("the second consume runs");
        let after = stored(&pool, created.id).await;
        assert_eq!(after.uses, 2, "the count still rises");
        assert!(
            !after.used,
            "a date-limited invite is spent by its date, not by use"
        );
        assert!(after.is_valid(today()), "and it serves on until the date");
        assert!(
            !after.is_valid(day_shift(2)),
            "the day after the expiry it is spent"
        );
    }

    #[tokio::test]
    async fn a_reusable_date_limited_invite_marks_used_at_its_cap() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let params = InviteParams {
            expires_at: Some(day_shift(7)),
            max_use: Some(1),
        };
        let created = create(&pool, &params).await.expect("the insert runs");
        consume(&pool, created.id).await.expect("the consume runs");
        let after = stored(&pool, created.id).await;
        assert_eq!(after.uses, 1, "the count rose");
        assert!(after.used, "the cap of one is reached at once");
        assert!(!after.is_valid(today()), "the invite is exhausted");
    }

    #[tokio::test]
    async fn consume_of_a_vanished_invite_is_an_error() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let err = consume(&pool, Uuid::now_v7())
            .await
            .expect_err("no invite carries this id");
        assert!(
            matches!(err, AppError::Internal(_)),
            "a missed update is the internal arm, got {err}"
        );
    }

    #[tokio::test]
    async fn list_answers_every_invite_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            list(&pool).await.expect("the list runs").is_empty(),
            "fresh table"
        );
        let first = create(&pool, &InviteParams::one_time())
            .await
            .expect("the first invite inserts");
        let second = create(
            &pool,
            &InviteParams {
                expires_at: Some(day_shift(1)),
                max_use: Some(3),
            },
        )
        .await
        .expect("the second invite inserts");

        let listed = list(&pool).await.expect("the list runs");
        let ids: Vec<Uuid> = listed.iter().map(|invite| invite.id).collect();
        assert_eq!(ids.len(), 2, "both invites are listed");
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "ids ascending: creation order on v7 text");
        assert!(listed.contains(&first), "the first row is there unchanged");
        assert!(listed.contains(&second), "and the second");
    }

    #[tokio::test]
    async fn revoke_deletes_the_named_invite_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let doomed = create(&pool, &InviteParams::one_time())
            .await
            .expect("the doomed invite inserts");
        let kept = create(&pool, &InviteParams::one_time())
            .await
            .expect("the kept invite inserts");

        assert!(
            revoke(&pool, &doomed.token).await.expect("the delete runs"),
            "the first revoke hit the row"
        );
        assert!(
            !revoke(&pool, &doomed.token)
                .await
                .expect("the second delete runs"),
            "revoking twice removes nothing the second time"
        );
        assert!(
            get_by_token(&pool, &doomed.token)
                .await
                .expect("the lookup runs")
                .is_none(),
            "the revoked invite is gone"
        );
        let listed = list(&pool).await.expect("the list runs");
        assert_eq!(listed.len(), 1, "one invite is left");
        assert_eq!(listed[0].id, kept.id, "and it is the kept one");
    }
}
