//! Issuing and reading access tokens: [`create_token`] mints a pair,
//! [`get_valid_token`] resolves a presented credential to its live owner,
//! the by-id read returns a row as stored, and [`consume_refresh_token`]
//! redeems a refresh token exactly once.

use std::time::Duration;

use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use super::{DEFAULT_TOKEN_TTL, NewToken, OauthToken};
use crate::{
    error::AppResult,
    models::user::{self, User},
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token},
    },
};

/// The `INSERT` behind [`create_token`]: both credential columns are
/// minted by the caller of the statement, the expiry is computed from
/// the bound lifetime by the database clock, and `RETURNING *` brings
/// back the row with its timestamps.
const INSERT_TOKEN_SQL: &str = "INSERT INTO oauth_tokens (
     id, token, refresh_token, user_id, app_id, scopes, valid_until
) VALUES ($1, $2, $3, $4, $5, $6, now() + $7) RETURNING *";

/// Inserts a token pair for `new_token` and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); `token` and `refresh_token`
/// are two independent 43-character random tokens (Pleroma's
/// `Token.Utils.generate_token`), and `valid_until` is the database's
/// `now()` plus the requested lifetime (default
/// [`DEFAULT_TOKEN_TTL`]). Both token columns are unique, so a minted
/// collision is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
///
/// Runs on the caller's executor (a pool or a transaction), so the refresh
/// grant can mint the replacement pair in the transaction that consumed the
/// old one.
pub async fn create_token<'e, E>(executor: E, new_token: &NewToken) -> AppResult<OauthToken>
where
    E: Executor<'e, Database = Postgres>,
{
    let ttl = new_token.ttl.unwrap_or(DEFAULT_TOKEN_TTL);
    let token = sqlx::query_as::<_, OauthToken>(INSERT_TOKEN_SQL)
        .bind(new_id())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(new_token.user_id)
        .bind(new_token.app_id)
        .bind(new_token.scopes.as_str())
        .bind(ttl)
        .fetch_one(executor)
        .await?;
    Ok(token)
}

/// Join row for [`get_valid_token`]: every [`OauthToken`] column plus
/// the owning user's `is_active` flag, which a `LEFT JOIN` leaves `NULL`
/// for user-unbound rows.
#[derive(sqlx::FromRow)]
struct ValidTokenRow {
    /// The token row itself, flattened over the `t.*` columns.
    #[sqlx(flatten)]
    token: OauthToken,
    /// `users.is_active` of the joined user; `None` for app-only tokens.
    user_is_active: Option<bool>,
}

/// Looks up the access token `token` and, when it is still valid,
/// returns it with its user (`None` for an app-only token).
///
/// A token is valid when its row exists, `valid_until` lies in the
/// future, and — for user-bound tokens — the joined user is still
/// active. Anything else (unknown token, expired, deactivated user) is
/// `None`; a suspended account's bearer tokens stop working the moment
/// the flag flips.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_valid_token(
    pool: &PgPool,
    token: &str,
) -> AppResult<Option<(OauthToken, Option<User>)>> {
    let row = sqlx::query_as::<_, ValidTokenRow>(
        "SELECT t.*, u.is_active AS user_is_active
           FROM oauth_tokens t
           LEFT JOIN users u ON u.id = t.user_id
          WHERE t.token = $1 AND t.valid_until > now()",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    let Some(ValidTokenRow {
        token: found,
        user_is_active,
    }) = row
    else {
        return Ok(None);
    };
    // An app-only token (NULL user_id) answers NULL for the joined flag
    // and passes; a user-bound one needs a live, active user.
    if found.user_id.is_some() && !user_is_active.unwrap_or_default() {
        return Ok(None);
    }
    let user = match found.user_id {
        Some(user_id) => user::get_by_id(pool, user_id).await?,
        None => None,
    };
    Ok(Some((found, user)))
}

/// The `SELECT` behind [`get_by_id`]: the token row verbatim by primary
/// key — no validity filter, because the caller (the `push.deliver` job)
/// reads the token string a subscription was registered for, expiry or not.
const GET_BY_ID_SQL: &str = "SELECT * FROM oauth_tokens WHERE id = $1";

/// The access token row with id `id`, `None` when it is gone.
///
/// The [`push.deliver` job](crate::jobs::web_push) reads the token string
/// the payload carries through its subscription's `token_id`; the foreign
/// key's cascade means a live subscription normally has one.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<OauthToken>> {
    let row = sqlx::query_as::<_, OauthToken>(GET_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The `DELETE` behind [`consume_refresh_token`]: the row of this refresh
/// token and application, but only while its `valid_until` lies within the
/// bound grace interval of the database clock. The cast pins `$3` to an
/// interval, since `timestamptz - unknown` would otherwise resolve to the
/// timestamp difference.
const CONSUME_REFRESH_TOKEN_SQL: &str = "DELETE FROM oauth_tokens
         WHERE refresh_token = $1 AND app_id = $2 AND valid_until > now() - $3::interval
     RETURNING *";

/// Takes the pair holding `refresh_token` that was issued to `app_id`,
/// deleting it in the statement that returns it.
///
/// A refresh token can therefore be redeemed exactly once: of two
/// concurrent redemptions the second finds no row and gets `None`.
///
/// A pair whose `valid_until` is more than `grace` in the past is not
/// taken (callers pass [`REFRESH_GRACE`](super::REFRESH_GRACE)) — the
/// bound the sweep uses to reclaim such rows, so the two cannot drift. An
/// unknown token, another application's token and a lapsed pair all answer
/// `None` and leave the stored rows alone.
///
/// Runs on the caller's executor (or transaction), so the deletion can land
/// together with the insert of the replacement pair, and roll back with it
/// when the owner turns out not to be ready.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn consume_refresh_token<'e, E>(
    executor: E,
    refresh_token: &str,
    app_id: Uuid,
    grace: Duration,
) -> AppResult<Option<OauthToken>>
where
    E: Executor<'e, Database = Postgres>,
{
    let taken = sqlx::query_as::<_, OauthToken>(CONSUME_REFRESH_TOKEN_SQL)
        .bind(refresh_token)
        .bind(app_id)
        .bind(grace)
        .fetch_optional(executor)
        .await?;
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::test_support::*;
    use super::super::{REFRESH_GRACE, list_for_user};
    use super::*;

    #[tokio::test]
    async fn create_token_round_trips_and_validates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "alice").await;
        let app = insert_app(&pool, "Client").await;
        let token = create_token(
            &pool,
            &NewToken {
                scopes: "read write follow".to_owned(),
                ..new_token(Some(alice.id), app.id)
            },
        )
        .await
        .unwrap();

        assert!(token.id.get_version_num() == 7, "id is a UUIDv7");
        assert!(
            token.token.len() == 43 && is_url_safe(&token.token),
            "the access token is a 43-char URL-safe random token"
        );
        assert!(
            token.refresh_token.len() == 43 && is_url_safe(&token.refresh_token),
            "the refresh token is one too"
        );
        assert_ne!(token.token, token.refresh_token, "the two tokens differ");
        assert_eq!(
            token.user_id,
            Some(alice.id),
            "the user binding round trips"
        );
        assert_eq!(token.app_id, app.id, "the app binding round trips");
        assert_eq!(token.scopes, "read write follow", "scopes round trip");
        let days = DEFAULT_TOKEN_TTL.as_secs();
        let remaining = token
            .valid_until
            .signed_duration_since(Utc::now())
            .num_seconds();
        assert!(
            (i64::try_from(days).unwrap() - 60..=i64::try_from(days).unwrap() + 60)
                .contains(&remaining),
            "the default lifetime is about 30 days out, got {remaining}s"
        );
        assert!(
            token.inserted_at <= token.updated_at,
            "the database stamps both timestamps"
        );

        let (found, user) = get_valid_token(&pool, &token.token)
            .await
            .unwrap()
            .expect("a fresh token is valid");
        assert_eq!(found.id, token.id, "the token is found by its text");
        assert_eq!(found.token, token.token, "the secret round trips");
        assert_eq!(
            user.map(|loaded| loaded.id),
            Some(alice.id),
            "the owning user is loaded with it"
        );
        let listed = list_for_user(&pool, alice.id).await.unwrap();
        assert_eq!(listed.len(), 1, "the user's list holds the token");
        assert_eq!(listed.first().map(|t| t.id), Some(token.id), "same row");
    }

    #[tokio::test]
    async fn custom_ttl_is_applied() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = insert_app(&pool, "Dial-Down").await;
        let token = create_token(
            &pool,
            &NewToken {
                ttl: Some(Duration::from_secs(60)),
                ..new_token(None, app.id)
            },
        )
        .await
        .unwrap();
        let remaining = token
            .valid_until
            .signed_duration_since(Utc::now())
            .num_seconds();
        assert!(
            (30..=70).contains(&remaining),
            "a one-minute ttl expires a minute out, got {remaining}s"
        );
    }

    /// [`get_by_id`] hands back the row behind a primary key — an expired
    /// one included, since the read is the push job's, not the auth
    /// gate's — and `None` for an id nobody holds.
    #[tokio::test]
    async fn get_by_id_reads_the_stored_row_whatever_its_expiry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = insert_app(&pool, "By-Id").await;
        let created = create_token(&pool, &new_token(None, app.id)).await.unwrap();

        let found = get_by_id(&pool, created.id).await.unwrap();
        assert_eq!(
            found.map(|row| (row.id, row.token)),
            Some((created.id, created.token.clone())),
            "the row reads back verbatim"
        );

        expire_token(&pool, created.id).await;
        let stale = get_by_id(&pool, created.id)
            .await
            .unwrap()
            .expect("an expired row is still the row behind the id");
        assert_eq!(stale.token, created.token, "no validity filter applies");
        assert!(
            get_valid_token(&pool, &created.token)
                .await
                .unwrap()
                .is_none(),
            "the validity read, by contrast, refuses it"
        );
        assert!(
            get_by_id(&pool, new_id()).await.unwrap().is_none(),
            "an unknown id names no token"
        );
    }

    #[tokio::test]
    async fn app_only_tokens_validate_without_a_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = insert_app(&pool, "Machine").await;
        let token = create_token(&pool, &new_token(None, app.id)).await.unwrap();
        let (found, user) = get_valid_token(&pool, &token.token)
            .await
            .unwrap()
            .expect("a client_credentials token is valid on its own");
        assert_eq!(found.id, token.id, "the row comes back");
        assert!(user.is_none(), "there is no user to load");
        let nobody = new_id();
        assert!(
            list_for_user(&pool, nobody).await.unwrap().is_empty(),
            "an app-only token belongs to no user's list"
        );
    }

    #[tokio::test]
    async fn expired_tokens_are_not_returned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "carol").await;
        let app = insert_app(&pool, "Stale").await;
        let token = create_token(&pool, &new_token(Some(alice.id), app.id))
            .await
            .unwrap();
        expire_token(&pool, token.id).await;
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_none(),
            "a token past valid_until is refused"
        );
        let redeemed = consume_refresh_token(&pool, &token.refresh_token, app.id, REFRESH_GRACE)
            .await
            .unwrap()
            .expect("an hour past valid_until is well inside the grace window");
        assert_eq!(redeemed.id, token.id, "the lapsed pair is still redeemable");
    }

    /// Back-dates the pair `id` so that its `valid_until` lies `days` days
    /// in the past.
    async fn lapse_days_ago(pool: &PgPool, id: Uuid, days: i32) {
        let outcome = sqlx::query(
            "UPDATE oauth_tokens SET valid_until = now() - make_interval(days => $2) WHERE id = $1",
        )
        .bind(id)
        .bind(days)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the back-dating hit the row");
    }

    /// Rows of `oauth_tokens` holding `refresh_token`.
    async fn rows_with_refresh_token(pool: &PgPool, refresh_token: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM oauth_tokens WHERE refresh_token = $1")
            .bind(refresh_token)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn consume_refresh_token_returns_the_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "once").await;
        let app = insert_app(&pool, "Single-Use").await;
        let token = create_token(&pool, &new_token(Some(alice.id), app.id))
            .await
            .unwrap();

        let first = consume_refresh_token(&pool, &token.refresh_token, app.id, REFRESH_GRACE)
            .await
            .unwrap()
            .expect("the first redemption takes the row");
        assert_eq!(first.id, token.id, "it is the pair's own row");
        assert_eq!(first.token, token.token, "returned with its access token");
        assert_eq!(
            rows_with_refresh_token(&pool, &token.refresh_token).await,
            0,
            "the row is deleted by the same statement"
        );
        assert!(
            consume_refresh_token(&pool, &token.refresh_token, app.id, REFRESH_GRACE)
                .await
                .unwrap()
                .is_none(),
            "the second redemption finds nothing"
        );
    }

    #[tokio::test]
    async fn consume_refresh_token_ignores_another_apps_token() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "scoped").await;
        let owner = insert_app(&pool, "Owner").await;
        let stranger = insert_app(&pool, "Stranger").await;
        let token = create_token(&pool, &new_token(Some(alice.id), owner.id))
            .await
            .unwrap();

        assert!(
            consume_refresh_token(&pool, &token.refresh_token, stranger.id, REFRESH_GRACE)
                .await
                .unwrap()
                .is_none(),
            "another app's id takes nothing"
        );
        assert_eq!(
            rows_with_refresh_token(&pool, &token.refresh_token).await,
            1,
            "and leaves the row in place"
        );
        assert!(
            consume_refresh_token(&pool, &token.refresh_token, owner.id, REFRESH_GRACE)
                .await
                .unwrap()
                .is_some(),
            "the owning app still redeems it"
        );
    }

    #[tokio::test]
    async fn consume_refresh_token_enforces_the_grace_window() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "grace-window").await;
        let app = insert_app(&pool, "Window").await;
        let stale = create_token(&pool, &new_token(Some(alice.id), app.id))
            .await
            .unwrap();
        let recent = create_token(&pool, &new_token(Some(alice.id), app.id))
            .await
            .unwrap();
        lapse_days_ago(&pool, stale.id, 8).await;
        lapse_days_ago(&pool, recent.id, 6).await;

        assert!(
            consume_refresh_token(&pool, &stale.refresh_token, app.id, REFRESH_GRACE)
                .await
                .unwrap()
                .is_none(),
            "a pair lapsed eight days ago is past the grace window"
        );
        assert_eq!(
            rows_with_refresh_token(&pool, &stale.refresh_token).await,
            1,
            "the refusal deletes nothing"
        );
        let redeemed = consume_refresh_token(&pool, &recent.refresh_token, app.id, REFRESH_GRACE)
            .await
            .unwrap()
            .expect("a pair lapsed six days ago is inside it");
        assert_eq!(
            redeemed.id, recent.id,
            "the recent pair is the one redeemed"
        );
    }

    #[tokio::test]
    async fn a_rolled_back_consumption_keeps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "rollback").await;
        let app = insert_app(&pool, "Undo").await;
        let token = create_token(&pool, &new_token(Some(alice.id), app.id))
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        let redeemed = consume_refresh_token(&mut *tx, &token.refresh_token, app.id, REFRESH_GRACE)
            .await
            .unwrap();
        assert!(redeemed.is_some(), "the transaction sees its own deletion");
        tx.rollback().await.unwrap();
        assert_eq!(
            rows_with_refresh_token(&pool, &token.refresh_token).await,
            1,
            "a rollback puts the pair back"
        );
    }

    #[tokio::test]
    async fn deactivated_users_tokens_are_not_returned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = insert_user(&pool, "dave").await;
        let app = insert_app(&pool, "Suspension").await;
        let token = create_token(&pool, &new_token(Some(dave.id), app.id))
            .await
            .unwrap();
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_some(),
            "freshly issued, the token works"
        );
        deactivate_user(&pool, dave.id).await;
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_none(),
            "a suspended user's bearer token stops working"
        );
    }
}
