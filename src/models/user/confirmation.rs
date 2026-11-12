//! The email-confirmation flow: the token an unconfirmed account carries,
//! the letter that carries the link, and the link's two writers.
//!
//! The reference is Pleroma's `User.confirmation_changeset`,
//! `User.send_confirmation_email` and `User.confirm`
//! (`lib/pleroma/user.ex`), plus the `TokenController.confirm_email` action
//! (`lib/pleroma/web/o_auth/token_controller.ex`) and
//! `PleromaAPI.AccountController.confirmation_resend`
//! (`lib/pleroma/web/pleroma_api/controllers/account_controller.ex`) that
//! drive them. The state lives in three `users` columns —
//! [`email_confirmed`](super::User::email_confirmed),
//! [`confirmation_token`](super::User::confirmation_token) and
//! [`confirmation_sent_at`](super::User::confirmation_sent_at) — and this
//! module owns every write to them:
//!
//! * [`send`] mints a fresh token (32 CSPRNG bytes as 43 URL-safe
//!   characters, the shape [`random_token`] gives), stores it with the send
//!   time and queues [`registration_confirmation`] carrying the link
//!   [`link`] builds — always *queued*, never sent
//!   in-request, per `docs/ARCHITECTURE.md` decision 11.
//! * [`confirm`] turns the account on for the one token that matches it,
//!   clearing the token as it goes.
//! * [`unconfirmed_by_email`] is the lookup `POST
//!   /api/v1/pleroma/accounts/confirmation_resend` runs.
//!
//! Registration decides *whether* an account needs this at all from
//! `ACCOUNT_ACTIVATION_REQUIRED` (see
//! [`create_local_user`](super::register::create_local_user)); nothing here
//! reads the config's mode, so a resend's letter is owed by the row's own
//! unconfirmed state.

use sqlx::PgPool;
use uuid::Uuid;

use super::User;
use crate::{
    error::AppResult,
    mailer::{self, templates::registration_confirmation},
    state::AppState,
    util::token::{CREDENTIAL_BYTES, random_token},
};

/// The path a confirmation link points at, which
/// [`registration_confirmation`] hangs onto [`Config::base_url`]: Pleroma's
/// `"/account/confirm_email/:user_id/:token"` route
/// (`lib/pleroma/web/router.ex`), the path the `confirm_email` handler is
/// served on.
///
/// [`Config::base_url`]: crate::config::Config::base_url
const CONFIRMATION_PATH: &str = "api/account/confirm_email";

/// [`send`]: stores a freshly minted token and stamps the send time. An id
/// no account carries matches nothing — the caller read the row moments
/// ago, and a letter for a vanished account is nobody's error.
const ROTATE_TOKEN_SQL: &str = "UPDATE users
      SET confirmation_token = $2, confirmation_sent_at = now(), updated_at = now()
    WHERE id = $1";

/// [`confirm`]: the one write that activates an address, and the token's
/// death. The `WHERE` clause is Pleroma's check — local, still unconfirmed,
/// token matched exactly — so a stale or forged token changes nothing and
/// the answered `false` says so.
const CONFIRM_SQL: &str = "UPDATE users
      SET email_confirmed = true, confirmation_token = NULL,
          confirmation_sent_at = NULL, updated_at = now()
    WHERE id = $1
      AND local
      AND NOT email_confirmed
      AND confirmation_token = $2";

/// [`unconfirmed_by_email`]: the local, still-unconfirmed account one email
/// address belongs to — the only row a resend may re-mail. The bind is cast
/// to citext so the address matches case-insensitively like the column.
const SELECT_UNCONFIRMED_BY_EMAIL_SQL: &str = "SELECT * FROM users
     WHERE email = $1::citext AND local AND NOT email_confirmed";

/// The confirmation link for `user_id` carrying `token`: the path
/// [`registration_confirmation`] hangs onto [`Config::base_url`], which is
/// the URL `GET /api/account/confirm_email/{user_id}/{token}` answers.
///
/// [`Config::base_url`]: crate::config::Config::base_url
#[must_use]
pub fn link(user_id: Uuid, token: &str) -> String {
    format!("{CONFIRMATION_PATH}/{user_id}/{token}")
}

/// Mints a fresh confirmation token for `user`, stores it with the send time
/// and queues the letter carrying [`link`].
///
/// The token is never returned to a caller: only the queued email carries it.
/// While `SMTP_HOST` is unset the queue drops the letter ([`deliver_later`]
/// logs it), but the token is stored all the same, so an operator who
/// configures a relay can resend.
///
/// [`deliver_later`]: mailer::deliver_later
///
/// # Errors
///
/// Any database failure — the token write or the queue insert — arrives
/// through `?` as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn send(state: &AppState, user: &User) -> AppResult<()> {
    let Some(email) = user.email.as_deref() else {
        return Ok(());
    };
    let token = random_token(CREDENTIAL_BYTES);
    let _rotated = sqlx::query(ROTATE_TOKEN_SQL)
        .bind(user.id)
        .bind(&token)
        .execute(&state.db)
        .await?;
    let letter = registration_confirmation(&state.config, email, &link(user.id, &token));
    mailer::deliver_later(state, letter).await
}

/// Confirms the email address of the local account `user_id` whose pending
/// [`confirmation_token`](User::confirmation_token) is exactly `token`:
/// clears the token and answers `true`.
///
/// Any other pairing — unknown id, an already-confirmed account, a stale or
/// forged token — changes nothing and answers `false`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn confirm(pool: &PgPool, user_id: Uuid, token: &str) -> AppResult<bool> {
    let outcome = sqlx::query(CONFIRM_SQL)
        .bind(user_id)
        .bind(token)
        .execute(pool)
        .await?;
    Ok(outcome.rows_affected() > 0)
}

/// The local account whose email is `email` and whose address is still
/// unconfirmed, `None` when no such account exists.
///
/// The resend endpoint's whole question, answered the same way whether or
/// not anything was found.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn unconfirmed_by_email(pool: &PgPool, email: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>(SELECT_UNCONFIRMED_BY_EMAIL_SQL)
        .bind(email)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::{
        config::Config,
        models::user::{Flag, NewUser, insert},
        util::token::random_token,
    };

    /// An [`AppState`] over a test slot, with `SMTP_HOST` set so [`send`]
    /// really queues, and its recording mailbox.
    async fn state_with_mailbox() -> (crate::test_db::TestDb, AppState) {
        use std::sync::Arc;

        use crate::{http_client::HttpClient, mailer::MemoryTransport, uploads::UploaderBackend};

        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut config = Config::for_tests();
        config.smtp_host = Some("smtp.example.org".to_owned());
        let config = Arc::new(config);
        let state = AppState {
            db: db.pool().clone(),
            hub: crate::streaming::hub::Hub::new(db.pool()),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(MemoryTransport::new()),
        };
        (db, state)
    }

    /// A local account named `nickname` at `<nickname>@example.org`,
    /// confirmed unless `confirmed` says otherwise.
    async fn account(pool: &PgPool, nickname: &str, confirmed: bool) -> User {
        insert(
            pool,
            &NewUser {
                email: Some(format!("{nickname}@example.org")),
                email_confirmed: Flag::new(confirmed),
                ..NewUser::local(nickname, &format!("https://test.example/users/{nickname}"))
            },
        )
        .await
        .expect("user fixture inserts")
    }

    /// The stored row behind `id`, re-read to prove what a write changed.
    async fn stored(pool: &PgPool, id: Uuid) -> User {
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .expect("the read runs")
            .expect("the account exists")
    }

    /// The live mailer job, if one is queued.
    async fn queued_letter(pool: &PgPool) -> Option<serde_json::Value> {
        sqlx::query_as::<_, (serde_json::Value,)>(
            "SELECT args FROM jobs WHERE queue = 'mailer' ORDER BY inserted_at",
        )
        .fetch_optional(pool)
        .await
        .expect("the job read runs")
        .map(|(args,)| args)
    }

    /// A token read straight off the row, for assertions about what was
    /// stored without going through a `Debug` impl that redacts it.
    async fn stored_token(pool: &PgPool, id: Uuid) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT confirmation_token FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("the token read runs")
        .flatten()
    }

    #[test]
    fn link_carries_the_account_and_the_token() {
        let id = Uuid::now_v7();
        assert_eq!(
            link(id, "tok3n"),
            format!("api/account/confirm_email/{id}/tok3n"),
            "the path Pleroma serves, with both segments"
        );
    }

    #[tokio::test]
    async fn send_stores_a_fresh_token_and_queues_the_letter() {
        let (db, state) = state_with_mailbox().await;
        let pool = db.pool();
        let user = account(pool, "confirm-me", false).await;
        let before = Utc::now();

        send(&state, &user).await.expect("the send runs");

        let token = stored_token(pool, user.id)
            .await
            .expect("a token was stored");
        assert_eq!(token.len(), 43, "32 CSPRNG bytes, base64url");
        let row = stored(pool, user.id).await;
        assert!(
            !row.email_confirmed.get(),
            "the letter alone does not confirm"
        );
        let sent_at = row.confirmation_sent_at.expect("the send time was stamped");
        assert!(sent_at >= before, "the stamp is not in the past");
        let args = queued_letter(pool).await.expect("a letter is queued");
        assert_eq!(args["to"], "confirm-me@example.org", "to the account");
        let body = args["text_body"].as_str().expect("a text body");
        assert!(
            body.contains(&format!(
                "https://test.example/api/account/confirm_email/{}/{}",
                user.id, token
            )),
            "the queued letter carries the absolute link: {body}"
        );
    }

    #[tokio::test]
    async fn send_rotates_the_token_it_replaces() {
        let (db, state) = state_with_mailbox().await;
        let pool = db.pool();
        let user = account(pool, "rotate-me", false).await;
        let superseded = random_token(CREDENTIAL_BYTES);
        let _stored = sqlx::query("UPDATE users SET confirmation_token = $2 WHERE id = $1")
            .bind(user.id)
            .bind(&superseded)
            .execute(pool)
            .await
            .expect("the superseded token is stored");

        send(&state, &user).await.expect("the send runs");

        let fresh = stored_token(pool, user.id)
            .await
            .expect("a token was stored");
        assert_ne!(fresh, superseded, "the resend replaces the old token");
        assert!(
            !confirm(pool, user.id, &superseded)
                .await
                .expect("the confirm runs"),
            "the superseded token no longer confirms anything"
        );
    }

    #[tokio::test]
    async fn send_without_an_email_queues_nothing() {
        let (db, state) = state_with_mailbox().await;
        let pool = db.pool();
        let user = insert(
            pool,
            &NewUser {
                email_confirmed: Flag::new(false),
                ..NewUser::local("no-mail-here", "https://test.example/users/no-mail-here")
            },
        )
        .await
        .expect("the addressless account inserts");

        send(&state, &user).await.expect("nothing to send");

        assert!(
            queued_letter(pool).await.is_none(),
            "an account with no address gets no letter"
        );
        assert!(
            stored_token(pool, user.id).await.is_none(),
            "and no token is minted for a letter that cannot go out"
        );
    }

    #[tokio::test]
    async fn confirm_activates_the_address_once_and_clears_the_token() {
        let (db, state) = state_with_mailbox().await;
        let pool = db.pool();
        let user = account(pool, "alice-confirm", false).await;
        send(&state, &user).await.expect("the letter goes out");
        let token = stored_token(pool, user.id)
            .await
            .expect("the send stored a token");

        assert!(
            !confirm(pool, user.id, "not-the-token")
                .await
                .expect("the wrong token is refused"),
            "a forged token changes nothing"
        );
        assert!(
            !stored(pool, user.id).await.email_confirmed.get(),
            "and the account is still unconfirmed"
        );
        assert!(
            confirm(pool, user.id, &token)
                .await
                .expect("the right token confirms"),
            "the matching token confirms the address"
        );
        let row = stored(pool, user.id).await;
        assert!(row.email_confirmed.get(), "the account is confirmed");
        assert!(
            row.confirmation_token.is_none(),
            "the used token is cleared away"
        );
        assert!(
            row.confirmation_sent_at.is_none(),
            "and so is the send stamp"
        );
        assert!(
            !confirm(pool, user.id, &token)
                .await
                .expect("the second attempt runs"),
            "the same token never confirms twice"
        );
    }

    #[tokio::test]
    async fn confirm_refuses_the_unknown_and_the_remote() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        assert!(
            !confirm(pool, Uuid::now_v7(), "anything")
                .await
                .expect("the lookup of nobody runs"),
            "an unknown id confirms nothing"
        );
        let remote = insert(
            pool,
            &NewUser {
                confirmation_token: Some("remote-token".to_owned()),
                email_confirmed: Flag::new(false),
                ..NewUser::local(
                    "someone@remote.example",
                    "https://remote.example/users/someone",
                )
            },
        )
        .await
        .expect("the mirror fixture inserts");
        let _cleared = sqlx::query("UPDATE users SET local = false WHERE id = $1")
            .bind(remote.id)
            .execute(pool)
            .await
            .expect("the fixture is remote");
        assert!(
            !confirm(pool, remote.id, "remote-token")
                .await
                .expect("the remote lookup runs"),
            "a remote mirror confirms nothing"
        );
    }

    #[tokio::test]
    async fn unconfirmed_by_email_finds_only_the_account_still_pending() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let pending = account(pool, "pending-one", false).await;
        let done = account(pool, "confirmed-one", true).await;

        let found = unconfirmed_by_email(pool, "PENDING-ONE@example.org")
            .await
            .expect("the case-insensitive lookup runs");
        assert_eq!(found.map(|user| user.id), Some(pending.id), "citext folds");
        assert!(
            unconfirmed_by_email(pool, "confirmed-one@example.org")
                .await
                .expect("the lookup of a confirmed address runs")
                .is_none(),
            "a confirmed account is not pending: {:?}",
            stored(pool, done.id).await.email_confirmed
        );
        assert!(
            unconfirmed_by_email(pool, "nobody@example.org")
                .await
                .expect("the lookup of nobody runs")
                .is_none(),
            "an unknown address finds nothing"
        );
    }

    #[tokio::test]
    async fn the_confirmation_columns_start_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let user = account(db.pool(), "never-confirmed", true).await;
        assert!(
            user.confirmation_token.is_none(),
            "an account that never went through the flow has no token"
        );
        assert!(user.confirmation_sent_at.is_none(), "and no send stamp");
    }
}
