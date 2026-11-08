//! The `web_push_subscriptions` table: the browser `PushSubscription` one
//! access token registered for Web Push delivery (WBE-144).
//!
//! The reference is Pleroma's `Pleroma.Web.Push.Subscription`
//! (`lib/pleroma/web/push/subscription.ex`). Pleroma looks a subscription
//! up by the `(user_id, token_id)` pair and deletes the old one before
//! creating a new one; here the unique `token_id` foreign key carries the
//! same promise — one subscription per access token — as the conflict
//! target of [`create`]'s upsert, so a re-create replaces the stored
//! subscription in place instead of needing a delete first. The Pleroma
//! schema's `data` map, whose only key the API ever writes is `alerts`,
//! is this table's `alerts` jsonb column beside the `policy` column
//! Mastodon's subscription data adds.
//!
//! The subscription belongs to a *token*, not merely to a user: two
//! tokens of one account are two subscriptions, and every read, update and
//! delete here is keyed by `token_id` alone (whose owner the foreign key
//! already fixed). Deleting the user or the token takes the row down
//! through the cascades.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// The default [`policy`](WebPushSubscription::policy): everyone may wake
/// the subscription (Mastodon's `PushSubscription::DEFAULT_POLICY`).
pub const DEFAULT_POLICY: &str = "all";

/// [`create`]: the upsert behind a registration. A first write inserts;
/// a write that collides with the token's existing subscription replaces
/// its endpoint, keys, alerts and policy in place (Pleroma's
/// delete-then-create, expressed as the unique `token_id` conflict target)
/// and restamps `updated_at`. `RETURNING *` brings the stored row back
/// with its id and the database-side timestamps.
const UPSERT_SQL: &str = "INSERT INTO web_push_subscriptions (
     id, user_id, token_id, endpoint, key_p256dh, key_auth, alerts, policy
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
    ON CONFLICT (token_id) DO UPDATE SET
          user_id     = EXCLUDED.user_id,
          endpoint    = EXCLUDED.endpoint,
          key_p256dh  = EXCLUDED.key_p256dh,
          key_auth    = EXCLUDED.key_auth,
          alerts      = EXCLUDED.alerts,
          policy      = EXCLUDED.policy,
          updated_at  = now()
      RETURNING *";

/// [`find`]: the subscription registered by one access token.
const FIND_SQL: &str = "SELECT * FROM web_push_subscriptions WHERE token_id = $1";

/// [`update_settings`]: rewrite the notification preferences of the token's
/// subscription, restamping `updated_at`; the endpoint and keys stay as
/// registered (Mastodon: only the data part can be updated).
const UPDATE_SQL: &str = "UPDATE web_push_subscriptions
                 SET alerts = $2, policy = $3, updated_at = now()
               WHERE token_id = $1
            RETURNING *";

/// [`delete_for_token`]: remove the token's subscription.
const DELETE_SQL: &str = "DELETE FROM web_push_subscriptions WHERE token_id = $1";

/// One row of `web_push_subscriptions`: the Web Push subscription of one
/// access token.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct WebPushSubscription {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); the
    /// replace-on-re-create upsert keeps it, so one token keeps one id.
    pub id: Uuid,
    /// The owning account, stored beside the token's for the cascade; the
    /// API always registers through a user token, so a row this module
    /// writes has one.
    pub user_id: Option<Uuid>,
    /// The access token this subscription belongs to; unique, the lookup
    /// key of every endpoint.
    pub token_id: Uuid,
    /// The browser callback URL notifications are pushed to.
    pub endpoint: String,
    /// Client ECDH public key (P-256), base64url.
    pub key_p256dh: String,
    /// Client auth secret, base64url.
    pub key_auth: String,
    /// The stored `{"<alert type>": <bool>}` object; the API always
    /// writes every known type, [`DEFAULT_POLICY`]'s counterpart on the
    /// data side.
    pub alerts: Value,
    /// Who may wake the subscription: `all`, `followed`, `follower` or
    /// `none`; the API validates the spelling before writing.
    pub policy: String,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; both writes restamp it.
    pub updated_at: DateTime<Utc>,
}

/// What [`create`] registers, besides the `id` it mints and the
/// timestamps the database defaults.
///
/// The whole subscription, already validated and normalized by the
/// endpoint that parsed the request.
#[derive(Clone, Debug)]
pub struct NewSubscription {
    /// The authenticated owner.
    pub user_id: Uuid,
    /// The access token the subscription is registered for.
    pub token_id: Uuid,
    /// The browser callback URL.
    pub endpoint: String,
    /// The client ECDH public key, base64url.
    pub key_p256dh: String,
    /// The client auth secret, base64url.
    pub key_auth: String,
    /// The alerts object to store: the API writes every known type.
    pub alerts: Value,
    /// The policy to store (validated, one of the four spellings).
    pub policy: String,
}

/// Registers `new` as `token_id`'s subscription, replacing any the token
/// already had.
///
/// Pleroma's `Subscription.create` behind a delete-`if`-`exists`, folded
/// into one upsert on the unique `token_id`: a replaced subscription
/// keeps the stored row's id (decision 4's one-row-per-token identity),
/// and a first registration mints a fresh `UUIDv7`.
///
/// # Errors
///
/// Any database failure (connection, encoding, a missing user or token
/// row under the foreign keys) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, new: &NewSubscription) -> AppResult<WebPushSubscription> {
    let row = sqlx::query_as::<_, WebPushSubscription>(UPSERT_SQL)
        .bind(Uuid::now_v7())
        .bind(new.user_id)
        .bind(new.token_id)
        .bind(&new.endpoint)
        .bind(&new.key_p256dh)
        .bind(&new.key_auth)
        .bind(&new.alerts)
        .bind(&new.policy)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The subscription registered by `token_id`, `None` when the token has
/// none (the endpoints' 404).
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn find(pool: &PgPool, token_id: Uuid) -> AppResult<Option<WebPushSubscription>> {
    let row = sqlx::query_as::<_, WebPushSubscription>(FIND_SQL)
        .bind(token_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Rewrites the token's subscription's notification preferences.
///
/// `None` when the token has no subscription to update; the endpoint and
/// keys are untouched — changing those means registering a new
/// subscription ([`create`]).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_settings(
    pool: &PgPool,
    token_id: Uuid,
    alerts: &Value,
    policy: &str,
) -> AppResult<Option<WebPushSubscription>> {
    let row = sqlx::query_as::<_, WebPushSubscription>(UPDATE_SQL)
        .bind(token_id)
        .bind(alerts)
        .bind(policy)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Removes `token_id`'s subscription, reporting whether one went; a token
/// that never registered (or already unregistered) deletes nothing and
/// reports `false`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_for_token(pool: &PgPool, token_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_SQL).bind(token_id).execute(pool).await?;
    Ok(deleted.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        models::{
            oauth_app::{NewOauthApp, create as create_app},
            oauth_token::{NewToken, create_token},
            user::{NewUser, insert},
        },
        util::id::new_id,
    };

    /// The alerts object the API stores: every known type, `follow` on.
    fn alerts() -> Value {
        json!({"follow": true, "mention": false})
    }

    /// A fresh local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> Uuid {
        insert(
            pool,
            &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
        )
        .await
        .expect("user fixture inserts")
        .id
    }

    /// A fresh token of user `owner_id`.
    async fn token(pool: &PgPool, owner_id: Uuid) -> Uuid {
        let app = create_app(
            pool,
            &NewOauthApp {
                client_name: "Push Tester".to_owned(),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: "push".to_owned(),
                website: None,
                user_id: Some(owner_id),
            },
        )
        .await
        .expect("app fixture inserts");
        create_token(
            pool,
            &NewToken {
                user_id: Some(owner_id),
                app_id: app.id,
                scopes: "push".to_owned(),
                ttl: None,
            },
        )
        .await
        .expect("token fixture inserts")
        .id
    }

    /// The registration the tests write: `token_id`'s subscription on
    /// `endpoint`, defaults elsewhere.
    fn subscription(owner_id: Uuid, token_id: Uuid, endpoint: &str) -> NewSubscription {
        NewSubscription {
            user_id: owner_id,
            token_id,
            endpoint: endpoint.to_owned(),
            key_p256dh: "BFraw-key".to_owned(),
            key_auth: "auth_secret".to_owned(),
            alerts: alerts(),
            policy: "all".to_owned(),
        }
    }

    /// A user with one fresh token: the pair every write in these tests
    /// registers against.
    async fn owner_and_token(pool: &PgPool, nickname: &str) -> (Uuid, Uuid) {
        let owner_id = user(pool, nickname).await;
        (owner_id, token(pool, owner_id).await)
    }

    /// [`create`] then [`find`] round-trips every column, with the
    /// database-stamped timestamps on the fresh row.
    #[tokio::test]
    async fn create_then_find_round_trips_the_subscription() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (owner_id, token_id) = owner_and_token(pool, "push-owner").await;
        let created = create(
            pool,
            &subscription(owner_id, token_id, "https://push.example/1"),
        )
        .await
        .expect("the registration writes");

        let found = find(pool, token_id)
            .await
            .expect("the lookup reaches the database")
            .expect("the token has a subscription");
        assert_eq!(found, created, "the lookup reads back the written row");
        assert_eq!(found.user_id, Some(owner_id), "the owner is stored");
        assert_eq!(found.endpoint, "https://push.example/1", "the endpoint");
        assert_eq!(found.key_p256dh, "BFraw-key", "the client public key");
        assert_eq!(found.key_auth, "auth_secret", "the client auth secret");
        assert_eq!(found.alerts, alerts(), "the alerts jsonb");
        assert_eq!(found.policy, "all", "the policy");
        assert_eq!(
            found.inserted_at, found.updated_at,
            "a fresh row is stamped once"
        );
    }

    /// [`find`] for a token nobody registered is `None`, not an error.
    #[tokio::test]
    async fn find_of_a_token_without_subscription_is_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let owner_id = user(pool, "push-quiet").await;
        let token_id = token(pool, owner_id).await;
        assert!(
            find(pool, token_id)
                .await
                .expect("the lookup reaches the database")
                .is_none(),
            "a fresh token has no subscription"
        );
    }

    /// A second [`create`] for the same token replaces the first in
    /// place: one row, the new endpoint, keys, alerts and policy, the
    /// original id kept and `updated_at` restamped.
    #[tokio::test]
    async fn create_replaces_the_tokens_previous_subscription() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (owner_id, token_id) = owner_and_token(pool, "push-twice").await;
        let first = create(
            pool,
            &subscription(owner_id, token_id, "https://push.example/1"),
        )
        .await
        .expect("the first registration writes");

        let mut second = subscription(owner_id, token_id, "https://push.example/2");
        second.key_p256dh = "BKother-key".to_owned();
        second.alerts = json!({"follow": false, "mention": true});
        second.policy = "follower".to_owned();
        let replaced = create(pool, &second)
            .await
            .expect("the second registration replaces");

        assert_eq!(replaced.id, first.id, "the token keeps its row");
        assert_eq!(replaced.endpoint, "https://push.example/2", "new endpoint");
        assert_eq!(replaced.key_p256dh, "BKother-key", "new client key");
        assert_eq!(replaced.alerts, json!({"follow": false, "mention": true}));
        assert_eq!(replaced.policy, "follower", "new policy");

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM web_push_subscriptions")
            .fetch_one(pool)
            .await
            .expect("the count runs");
        assert_eq!(rows.0, 1, "one row per token, whatever the writes");
    }

    /// [`update_settings`] rewrites the preferences and reports the new
    /// row; the endpoint and keys stay as registered.
    #[tokio::test]
    async fn update_settings_changes_preferences_and_keeps_the_keys() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (owner_id, token_id) = owner_and_token(pool, "push-editor").await;
        let created = create(
            pool,
            &subscription(owner_id, token_id, "https://push.example/1"),
        )
        .await
        .expect("the registration writes");

        let updated = update_settings(pool, token_id, &json!({"follow": false}), "none")
            .await
            .expect("the update reaches the database")
            .expect("the token has a subscription");
        assert_eq!(updated.id, created.id, "the same row, rewritten");
        assert_eq!(updated.endpoint, created.endpoint, "the endpoint stays");
        assert_eq!(updated.key_auth, created.key_auth, "the keys stay");
        assert_eq!(updated.alerts, json!({"follow": false}), "new alerts");
        assert_eq!(updated.policy, "none", "new policy");
        assert!(
            updated.updated_at >= created.updated_at,
            "the update restamps updated_at"
        );
    }

    /// [`update_settings`] for a token with no subscription changes
    /// nothing and reports `None`.
    #[tokio::test]
    async fn update_settings_without_a_subscription_is_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let owner_id = user(pool, "push-nothing").await;
        let token_id = token(pool, owner_id).await;
        assert!(
            update_settings(pool, token_id, &alerts(), "all")
                .await
                .expect("the update reaches the database")
                .is_none(),
            "there is no row to update"
        );
    }

    /// [`delete_for_token`] removes the row once: the first call reports
    /// `true` and empties the table for the token, the second `false`.
    #[tokio::test]
    async fn delete_for_token_removes_the_subscription_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (owner_id, token_id) = owner_and_token(pool, "push-leaver").await;
        let _created = create(
            pool,
            &subscription(owner_id, token_id, "https://push.example/1"),
        )
        .await
        .expect("the registration writes");

        assert!(
            delete_for_token(pool, token_id)
                .await
                .expect("the delete reaches the database"),
            "the first delete removes the row"
        );
        assert!(
            find(pool, token_id)
                .await
                .expect("the lookup runs")
                .is_none(),
            "and the token has no subscription after it"
        );
        assert!(
            !delete_for_token(pool, token_id)
                .await
                .expect("the second delete runs"),
            "a second delete removes nothing"
        );
    }

    /// Two tokens of one account are two subscriptions, keyed apart: each
    /// read, update and delete touches exactly its own token's row.
    #[tokio::test]
    async fn one_accounts_two_tokens_keep_two_subscriptions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let owner_id = user(pool, "push-multi").await;
        let one = token(pool, owner_id).await;
        let two = token(pool, owner_id).await;
        let _first = create(
            pool,
            &subscription(owner_id, one, "https://push.example/one"),
        )
        .await
        .expect("the first token registers");
        let _second = create(
            pool,
            &subscription(owner_id, two, "https://push.example/two"),
        )
        .await
        .expect("the second token registers");

        let updated = update_settings(pool, one, &json!({"follow": false}), "none")
            .await
            .expect("the update runs")
            .expect("the first token has a subscription");
        assert_eq!(updated.endpoint, "https://push.example/one");
        let untouched = find(pool, two)
            .await
            .expect("the lookup runs")
            .expect("the second token has a subscription");
        assert_eq!(
            untouched.policy, "all",
            "the other token's subscription keeps its settings"
        );
        assert!(
            delete_for_token(pool, one).await.expect("the delete runs"),
            "the first token's row goes"
        );
        assert!(
            find(pool, two).await.expect("the lookup runs").is_some(),
            "and the second token's row stays"
        );
    }

    /// Deleting the token cascades the subscription away with it (the
    /// unique foreign key doubles as the lifetime link).
    #[tokio::test]
    async fn deleting_the_token_deletes_the_subscription() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (owner_id, token_id) = owner_and_token(pool, "push-cascade").await;
        let _created = create(
            pool,
            &subscription(owner_id, token_id, "https://push.example/1"),
        )
        .await
        .expect("the registration writes");

        let _dropped = sqlx::query("DELETE FROM oauth_tokens WHERE id = $1")
            .bind(token_id)
            .execute(pool)
            .await
            .expect("the token delete runs");
        assert!(
            find(pool, token_id)
                .await
                .expect("the lookup runs")
                .is_none(),
            "the subscription cascaded away with its token"
        );
    }

    /// A bogus token id violates the foreign key rather than storing an
    /// orphan.
    #[tokio::test]
    async fn registering_for_an_unknown_token_fails_the_foreign_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let owner_id = user(pool, "push-stray").await;
        let outcome = create(
            pool,
            &subscription(owner_id, new_id(), "https://push.example/1"),
        )
        .await;
        assert!(outcome.is_err(), "no token row, no subscription");
    }
}
