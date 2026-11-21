//! The fixtures the `oauth_token` children's tests share: the user and app
//! rows tokens belong to, the token builder, the expiry and deactivation
//! updates, and the row counters the deletion tests read. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use super::NewToken;
use crate::models::{
    oauth_app::{NewOauthApp, OauthApp, create as create_app},
    user::{self, NewUser, User},
};

/// The instance origin the fixtures pretend to serve.
pub(super) const BASE: &str = "https://test.example";

/// Whether every character is in the URL-safe base64 alphabet.
pub(super) fn is_url_safe(token: &str) -> bool {
    token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Inserts a local user named `nickname` and returns the row.
pub(super) async fn insert_user(pool: &PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &NewUser::local(nickname, &user::local_ap_id(BASE, nickname)),
    )
    .await
    .unwrap()
}

/// Registers an app named `name` and returns it.
pub(super) async fn insert_app(pool: &PgPool, name: &str) -> OauthApp {
    create_app(
        pool,
        &NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "read".to_owned(),
            website: None,
            user_id: None,
        },
    )
    .await
    .unwrap()
}

/// A [`NewToken`] for `app_id` acting as `user_id`, default lifetime.
pub(super) fn new_token(user_id: Option<Uuid>, app_id: Uuid) -> NewToken {
    NewToken {
        user_id,
        app_id,
        scopes: "read".to_owned(),
        ttl: None,
    }
}

/// Moves token `id`'s `valid_until` an hour into the past.
pub(super) async fn expire_token(pool: &PgPool, id: Uuid) {
    let outcome = sqlx::query(
        "UPDATE oauth_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(outcome.rows_affected(), 1, "the expiry update hit the row");
}

/// Moves authorization `id`'s `valid_until` an hour into the past.
pub(super) async fn expire_authorization(pool: &PgPool, id: Uuid) {
    let outcome = sqlx::query(
        "UPDATE oauth_authorizations
            SET valid_until = now() - interval '1 hour'
          WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(outcome.rows_affected(), 1, "the expiry update hit the row");
}

/// Deactivates user `id`, the way a suspension would.
pub(super) async fn deactivate_user(pool: &PgPool, id: Uuid) {
    let outcome = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        outcome.rows_affected(),
        1,
        "the deactivation update hit the row"
    );
}

/// Deletes user `id`, firing the cascades.
pub(super) async fn delete_user(pool: &PgPool, id: Uuid) {
    let outcome = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(outcome.rows_affected(), 1, "the user delete hit the row");
}

/// Rows left in `oauth_tokens` with primary key `id`.
pub(super) async fn token_rows(pool: &PgPool, id: Uuid) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    rows
}

/// Rows left in `oauth_authorizations` with primary key `id`.
pub(super) async fn authorization_rows(pool: &PgPool, id: Uuid) -> i64 {
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_authorizations WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    rows
}

/// Whether authorization `id` is still stored unused.
pub(super) async fn authorization_unused(pool: &PgPool, id: Uuid) -> bool {
    let (used,): (bool,) = sqlx::query_as("SELECT used FROM oauth_authorizations WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    !used
}
