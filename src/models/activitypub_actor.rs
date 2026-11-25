//! The `activitypub_actors` table: `ActivityPub`'s own data about an account.
//!
//! One row per account (local or remote) that exists on `ActivityPub`. This is
//! `ActivityPub`'s own table under the `<protocol id>_...` naming rule
//! (WBE-832): `users` holds what every protocol shares, and each protocol
//! keeps the rest in tables named after it. The row holds the actor's
//! endpoints and collection ids, its signing key pair, the key id its
//! document names and when it was last fetched. `ap_id`, `domain`,
//! `actor_type` and the other columns client APIs render on every account
//! stay on `users`.
//!
//! [`ActivityPubActor::status`] says whether the account's `ActivityPub`
//! identity is claimed ([`ActorStatus::Active`]) or awaiting setup
//! ([`ActorStatus::Pending`]).
//!
//! This table is the only home of those columns. A local account's row is
//! written by `ActivityPub`'s claim ([`insert_new`], from
//! [`ActivityPubActorWrite::for_local_account`]), a remote actor's by
//! `federation::remote_actor::upsert_remote_user`. A reader of an account
//! holds the row ([`get`], [`get_many`]) or joins the table;
//! [`followers_address`] and [`featured_address`] name a collection from it.
//! Deleting the user deletes the row (the foreign key cascades).
//!
//! [`get`] and [`get_many`] read, [`get_many`] taking every id in one
//! statement so a caller holding a list never queries in a loop, and
//! [`upsert`] writes the whole row: it inserts, or on a conflict of
//! `user_id` overwrites every column and moves `updated_at`.

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{Executor, Postgres};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::user::{User, featured, followers, following, inbox, outbox},
    util::keys::KeyPair,
};

/// [`get`]: one account's row, or none without an `ActivityPub` identity.
const GET_SQL: &str = "SELECT user_id, status, inbox, shared_inbox, outbox, followers_url,
     following_url, featured_url, public_key, private_key, public_key_id,
     last_fetched_at, inserted_at, updated_at
  FROM activitypub_actors WHERE user_id = $1";

/// [`get_many`]: the rows of the named accounts, in the database's order.
///
/// An id with no row simply does not appear.
const GET_MANY_SQL: &str = "SELECT user_id, status, inbox, shared_inbox, outbox, followers_url,
     following_url, featured_url, public_key, private_key, public_key_id,
     last_fetched_at, inserted_at, updated_at
  FROM activitypub_actors WHERE user_id = ANY($1)";

/// [`upsert`]: inserts the row, or overwrites it on a conflict of `user_id`.
///
/// A conflict overwrites every written column and moves `updated_at`;
/// `inserted_at` is kept. `RETURNING` hands back the row as stored.
const UPSERT_SQL: &str = "INSERT INTO activitypub_actors (
         user_id, status, inbox, shared_inbox, outbox, followers_url,
         following_url, featured_url, public_key, private_key, public_key_id,
         last_fetched_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
ON CONFLICT (user_id) DO UPDATE SET
         status = EXCLUDED.status,
         inbox = EXCLUDED.inbox,
         shared_inbox = EXCLUDED.shared_inbox,
         outbox = EXCLUDED.outbox,
         followers_url = EXCLUDED.followers_url,
         following_url = EXCLUDED.following_url,
         featured_url = EXCLUDED.featured_url,
         public_key = EXCLUDED.public_key,
         private_key = EXCLUDED.private_key,
         public_key_id = EXCLUDED.public_key_id,
         last_fetched_at = EXCLUDED.last_fetched_at,
         updated_at = now()
  RETURNING user_id, status, inbox, shared_inbox, outbox, followers_url,
         following_url, featured_url, public_key, private_key, public_key_id,
         last_fetched_at, inserted_at, updated_at";

/// [`insert_new`]: inserts the row unless the account or key id already has one.
const INSERT_NEW_SQL: &str = "INSERT INTO activitypub_actors (
         user_id, status, inbox, shared_inbox, outbox, followers_url,
         following_url, featured_url, public_key, private_key, public_key_id,
         last_fetched_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
ON CONFLICT DO NOTHING";

/// Whether an account's `ActivityPub` identity is claimed (the
/// `activitypub_actors.status` column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ActorStatus {
    /// The account's `ActivityPub` identity is awaiting setup.
    Pending,
    /// The account's `ActivityPub` identity is claimed. The column default.
    Active,
}

/// One row of `activitypub_actors`: an account's `ActivityPub` data.
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ActivityPubActor {
    /// The account this row belongs to: the primary key, and the `users` row
    /// whose deletion removes this one.
    pub user_id: Uuid,
    /// Whether the account's `ActivityPub` identity is claimed.
    pub status: ActorStatus,
    /// `ActivityPub` inbox endpoint.
    pub inbox: Option<String>,
    /// Shared inbox endpoint.
    pub shared_inbox: Option<String>,
    /// `ActivityPub` outbox collection id.
    pub outbox: Option<String>,
    /// Followers collection id.
    pub followers_url: Option<String>,
    /// Following collection id.
    pub following_url: Option<String>,
    /// Featured (pinned) collection id.
    pub featured_url: Option<String>,
    /// Actor public signing key (PEM).
    pub public_key: Option<String>,
    /// Actor private signing key, local accounts only. Never in `Debug`
    /// output.
    pub private_key: Option<String>,
    /// The `publicKey.id` the actor's document names; unique when set.
    pub public_key_id: Option<String>,
    /// When this remote actor was last fetched.
    pub last_fetched_at: Option<DateTime<Utc>>,
    /// When the row was first written.
    pub inserted_at: DateTime<Utc>,
    /// When the row was last written.
    pub updated_at: DateTime<Utc>,
}

/// [`private_key`](Self::private_key) is a secret and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for ActivityPubActor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivityPubActor")
            .field("user_id", &self.user_id)
            .field("status", &self.status)
            .field("inbox", &self.inbox)
            .field("shared_inbox", &self.shared_inbox)
            .field("outbox", &self.outbox)
            .field("followers_url", &self.followers_url)
            .field("following_url", &self.following_url)
            .field("featured_url", &self.featured_url)
            .field("public_key", &self.public_key)
            .field("private_key", &"<redacted>")
            .field("public_key_id", &self.public_key_id)
            .field("last_fetched_at", &self.last_fetched_at)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// What [`upsert`] writes: every column of the row except the two
/// timestamps, which the database keeps (`inserted_at` at first write,
/// `updated_at` at every write).
#[derive(Clone, PartialEq, Eq)]
pub struct ActivityPubActorWrite {
    /// The account the row belongs to; must name a `users` row.
    pub user_id: Uuid,
    /// Whether the account's `ActivityPub` identity is claimed.
    pub status: ActorStatus,
    /// `ActivityPub` inbox endpoint.
    pub inbox: Option<String>,
    /// Shared inbox endpoint.
    pub shared_inbox: Option<String>,
    /// `ActivityPub` outbox collection id.
    pub outbox: Option<String>,
    /// Followers collection id.
    pub followers_url: Option<String>,
    /// Following collection id.
    pub following_url: Option<String>,
    /// Featured (pinned) collection id.
    pub featured_url: Option<String>,
    /// Actor public signing key (PEM).
    pub public_key: Option<String>,
    /// Actor private signing key, local accounts only. Never in `Debug`
    /// output.
    pub private_key: Option<String>,
    /// The `publicKey.id` the actor's document names; unique when set.
    pub public_key_id: Option<String>,
    /// When this remote actor was last fetched.
    pub last_fetched_at: Option<DateTime<Utc>>,
}

/// [`private_key`](Self::private_key) is a secret and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for ActivityPubActorWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivityPubActorWrite")
            .field("user_id", &self.user_id)
            .field("status", &self.status)
            .field("inbox", &self.inbox)
            .field("shared_inbox", &self.shared_inbox)
            .field("outbox", &self.outbox)
            .field("followers_url", &self.followers_url)
            .field("following_url", &self.following_url)
            .field("featured_url", &self.featured_url)
            .field("public_key", &self.public_key)
            .field("private_key", &"<redacted>")
            .field("public_key_id", &self.public_key_id)
            .field("last_fetched_at", &self.last_fetched_at)
            .finish()
    }
}

impl ActivityPubActorWrite {
    /// An active identity for `user_id` that holds nothing yet.
    ///
    /// Every endpoint, key and stamp is empty, for a caller that sets the few
    /// columns it knows with struct update syntax.
    #[must_use]
    pub const fn bare(user_id: Uuid) -> Self {
        Self {
            user_id,
            status: ActorStatus::Active,
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            public_key_id: None,
            last_fetched_at: None,
        }
    }

    /// The active identity of the local account `user` on the instance at `base_url`.
    ///
    /// The collections derive from the account's actor id, the shared inbox is
    /// the instance-wide `{base_url}/inbox`, and `keys` is the account's new
    /// signing key pair. The key id stays empty: a local actor's is its
    /// actor id plus `#main-key`.
    #[must_use]
    pub fn for_local_account(user: &User, base_url: &str, keys: KeyPair) -> Self {
        let ap_id = user.ap_id.as_str();
        Self {
            inbox: Some(inbox(ap_id)),
            shared_inbox: Some(format!("{base_url}/inbox")),
            outbox: Some(outbox(ap_id)),
            followers_url: Some(followers(ap_id)),
            following_url: Some(following(ap_id)),
            featured_url: Some(featured(ap_id)),
            public_key: Some(keys.public_pem),
            private_key: Some(keys.private_pem),
            ..Self::bare(user.id)
        }
    }
}

/// The `ActivityPub` row of `user_id`, or `None` when the account has none.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get<'e, E>(executor: E, user_id: Uuid) -> AppResult<Option<ActivityPubActor>>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as::<_, ActivityPubActor>(GET_SQL)
        .bind(user_id)
        .fetch_optional(executor)
        .await?;
    Ok(row)
}

/// The followers collection id on `user_id`'s row.
///
/// `None` when the account has no row or its row carries none; callers that
/// address a followers collection then build the id from the actor's own.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn followers_url<'e, E>(executor: E, user_id: Uuid) -> AppResult<Option<String>>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = get(executor, user_id).await?;
    Ok(row.and_then(|actor| actor.followers_url))
}

/// The `ActivityPub` rows of `user_ids`, in one statement.
///
/// Exactly the requested rows come back: an id with no row, and an id
/// repeated, each add nothing. The order is unspecified, so callers key the
/// result by [`ActivityPubActor::user_id`].
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many<'e, E>(executor: E, user_ids: &[Uuid]) -> AppResult<Vec<ActivityPubActor>>
where
    E: Executor<'e, Database = Postgres>,
{
    let rows = sqlx::query_as::<_, ActivityPubActor>(GET_MANY_SQL)
        .bind(user_ids)
        .fetch_all(executor)
        .await?;
    Ok(rows)
}

/// Writes `write` as the account's row and returns the row as stored.
///
/// The first write of a `user_id` inserts; a later one overwrites every
/// column in `write` (a `None` clears the column) and moves `updated_at`,
/// keeping `inserted_at`.
///
/// # Errors
///
/// A `user_id` that names no `users` row, or a `public_key_id` another
/// actor already has, fails the constraint; any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn upsert<'e, E>(
    executor: E,
    write: &ActivityPubActorWrite,
) -> AppResult<ActivityPubActor>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as::<_, ActivityPubActor>(UPSERT_SQL)
        .bind(write.user_id)
        .bind(write.status)
        .bind(write.inbox.as_deref())
        .bind(write.shared_inbox.as_deref())
        .bind(write.outbox.as_deref())
        .bind(write.followers_url.as_deref())
        .bind(write.following_url.as_deref())
        .bind(write.featured_url.as_deref())
        .bind(write.public_key.as_deref())
        .bind(write.private_key.as_deref())
        .bind(write.public_key_id.as_deref())
        .bind(write.last_fetched_at)
        .fetch_one(executor)
        .await?;
    Ok(row)
}

/// Writes `write` as a new row, or reports that one already exists.
///
/// Returns `true` when the row was inserted and `false` when the account
/// (`user_id`) or the `public_key_id` already has a row, leaving it untouched.
/// A conflict is an answer, not a database error, so it does not abort an
/// open transaction. This is how a claim of the `ActivityPub` identity
/// reports it is taken.
///
/// # Errors
///
/// A `user_id` that names no `users` row fails the foreign key; any database
/// failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn insert_new<'e, E>(executor: E, write: &ActivityPubActorWrite) -> AppResult<bool>
where
    E: Executor<'e, Database = Postgres>,
{
    let inserted = sqlx::query(INSERT_NEW_SQL)
        .bind(write.user_id)
        .bind(write.status)
        .bind(write.inbox.as_deref())
        .bind(write.shared_inbox.as_deref())
        .bind(write.outbox.as_deref())
        .bind(write.followers_url.as_deref())
        .bind(write.following_url.as_deref())
        .bind(write.featured_url.as_deref())
        .bind(write.public_key.as_deref())
        .bind(write.private_key.as_deref())
        .bind(write.public_key_id.as_deref())
        .bind(write.last_fetched_at)
        .execute(executor)
        .await?
        .rows_affected();
    Ok(inserted == 1)
}

/// The address naming the followers collection of actor `ap_id`.
///
/// It is the row's stored `followers_url` (what the publisher expands, and
/// what a peer's own documents name), with the [`followers`] construction
/// behind an account whose row is missing or carries none.
#[must_use]
pub fn followers_address(actor: Option<&ActivityPubActor>, ap_id: &str) -> String {
    actor
        .and_then(|row| row.followers_url.clone())
        .unwrap_or_else(|| followers(ap_id))
}

/// The address naming the followers collection of `user`, read from their row.
///
/// [`followers_address`] for a caller that holds no row yet.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn account_followers<'e, E>(executor: E, user: &User) -> AppResult<String>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = get(executor, user.id).await?;
    Ok(followers_address(row.as_ref(), &user.ap_id))
}

/// The address naming the featured (pinned) collection of actor `ap_id`.
///
/// It is the row's stored `featured_url` — the very url a peer's
/// `Add`/`Remove` handler matches the `target` against — with the
/// [`featured`] construction behind an account whose row is missing or
/// carries none.
#[must_use]
pub fn featured_address(actor: Option<&ActivityPubActor>, ap_id: &str) -> String {
    actor
        .and_then(|row| row.featured_url.clone())
        .unwrap_or_else(|| featured(ap_id))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        error::AppError,
        test_fixtures::{local_user, remote_user},
    };

    /// The secret the redaction tests look for.
    const SECRET_KEY: &str = "-----BEGIN PRIVATE KEY-----secret";

    /// A write for `user_id` with every column set to a distinct value.
    fn full_write(user_id: Uuid) -> ActivityPubActorWrite {
        ActivityPubActorWrite {
            user_id,
            status: ActorStatus::Pending,
            inbox: Some("https://remote.example/users/a/inbox".to_owned()),
            shared_inbox: Some("https://remote.example/inbox".to_owned()),
            outbox: Some("https://remote.example/users/a/outbox".to_owned()),
            followers_url: Some("https://remote.example/users/a/followers".to_owned()),
            following_url: Some("https://remote.example/users/a/following".to_owned()),
            featured_url: Some("https://remote.example/users/a/featured".to_owned()),
            public_key: Some("-----BEGIN PUBLIC KEY-----public".to_owned()),
            private_key: Some(SECRET_KEY.to_owned()),
            public_key_id: Some("https://remote.example/users/a#main-key".to_owned()),
            last_fetched_at: DateTime::from_timestamp(1_700_000_000, 0),
        }
    }

    /// A write for `user_id` with every optional column empty.
    fn bare_write(user_id: Uuid) -> ActivityPubActorWrite {
        ActivityPubActorWrite {
            user_id,
            status: ActorStatus::Active,
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            public_key_id: None,
            last_fetched_at: None,
        }
    }

    /// How many rows the table holds.
    async fn row_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM activitypub_actors")
            .fetch_one(pool)
            .await
            .expect("the rows count")
    }

    #[test]
    fn debug_redacts_the_private_key() {
        let write = full_write(Uuid::now_v7());
        let debug = format!("{write:?}");
        assert!(!debug.contains(SECRET_KEY), "the write must not leak it");
        assert!(debug.contains(r#"private_key: "<redacted>""#), "{debug}");
        assert!(debug.contains("inbox"), "everything else shows");
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = remote_user(&pool, "a").await;
        let write = full_write(user.id);

        let stored = upsert(&pool, &write).await.expect("the row writes");
        let read = get(&pool, user.id)
            .await
            .expect("the row reads")
            .expect("the row exists");

        assert_eq!(read, stored, "get returns what upsert returned");
        assert_eq!(read.user_id, write.user_id);
        assert_eq!(read.status, ActorStatus::Pending);
        assert_eq!(read.inbox, write.inbox);
        assert_eq!(read.shared_inbox, write.shared_inbox);
        assert_eq!(read.outbox, write.outbox);
        assert_eq!(read.followers_url, write.followers_url);
        assert_eq!(read.following_url, write.following_url);
        assert_eq!(read.featured_url, write.featured_url);
        assert_eq!(read.public_key, write.public_key);
        assert_eq!(read.private_key, write.private_key);
        assert_eq!(read.public_key_id, write.public_key_id);
        assert_eq!(read.last_fetched_at, write.last_fetched_at);
        assert_eq!(
            read.inserted_at, read.updated_at,
            "a first write stamps both timestamps alike"
        );
        let debug = format!("{read:?}");
        assert!(!debug.contains(SECRET_KEY), "the row must not leak it");
    }

    #[tokio::test]
    async fn get_of_an_account_without_a_row_is_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = local_user(&pool, "alice").await;

        assert!(
            get(&pool, user.id).await.expect("the read works").is_none(),
            "a user nothing wrote a row for has none"
        );
    }

    #[tokio::test]
    async fn a_second_upsert_overwrites_the_row_and_keeps_inserted_at() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = remote_user(&pool, "a").await;
        let first = upsert(&pool, &full_write(user.id))
            .await
            .expect("the insert writes");

        let second = upsert(&pool, &bare_write(user.id))
            .await
            .expect("the conflict updates");

        assert_eq!(row_count(&pool).await, 1, "still one row");
        assert_eq!(second.status, ActorStatus::Active, "status was replaced");
        assert!(second.inbox.is_none(), "a None clears the column");
        assert!(second.private_key.is_none(), "and the secret too");
        assert!(second.public_key_id.is_none(), "and the key id");
        assert!(second.last_fetched_at.is_none(), "and the fetch time");
        assert_eq!(second.inserted_at, first.inserted_at, "inserted_at is kept");
        assert!(
            second.updated_at > first.updated_at,
            "updated_at moved: {:?} then {:?}",
            first.updated_at,
            second.updated_at
        );
    }

    #[tokio::test]
    async fn get_many_returns_exactly_the_requested_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = remote_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob").await;
        let carol = remote_user(&pool, "carol").await;
        let dave = remote_user(&pool, "dave").await;
        for user in [&alice, &bob, &carol] {
            let _row = upsert(&pool, &bare_write(user.id))
                .await
                .expect("the fixture row writes");
        }

        let mut ids: Vec<Uuid> = get_many(&pool, &[alice.id, carol.id, dave.id, Uuid::now_v7()])
            .await
            .expect("the rows read")
            .into_iter()
            .map(|row| row.user_id)
            .collect();
        ids.sort();
        let mut expected = vec![alice.id, carol.id];
        expected.sort();
        assert_eq!(
            ids, expected,
            "alice and carol: bob was not asked for, dave and the stranger have no row"
        );

        assert!(
            get_many(&pool, &[])
                .await
                .expect("no ids is no error")
                .is_empty(),
            "asking for nobody answers nobody"
        );
    }

    #[tokio::test]
    async fn deleting_the_user_deletes_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let doomed = remote_user(&pool, "doomed").await;
        let kept = remote_user(&pool, "kept").await;
        for user in [&doomed, &kept] {
            let _row = upsert(&pool, &bare_write(user.id))
                .await
                .expect("the fixture row writes");
        }

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(doomed.id)
            .execute(&pool)
            .await
            .expect("the user deletes")
            .rows_affected();

        assert_eq!(deleted, 1, "one account went");
        assert!(
            get(&pool, doomed.id).await.unwrap().is_none(),
            "its row went with it"
        );
        assert!(
            get(&pool, kept.id).await.unwrap().is_some(),
            "the other row stayed"
        );
    }

    #[tokio::test]
    async fn a_row_for_no_stored_user_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let err = upsert(&pool, &bare_write(Uuid::now_v7()))
            .await
            .expect_err("the foreign key refuses");

        assert!(matches!(err, AppError::Database(_)), "{err}");
        assert_eq!(row_count(&pool).await, 0, "nothing was stored");
    }

    #[tokio::test]
    async fn status_rejects_other_values() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = local_user(&pool, "alice").await;

        let refused =
            sqlx::query("INSERT INTO activitypub_actors (user_id, status) VALUES ($1, $2)")
                .bind(user.id)
                .bind("suspended")
                .execute(&pool)
                .await;

        assert!(refused.is_err(), "the check constraint refuses 'suspended'");
        assert_eq!(row_count(&pool).await, 0, "nothing was stored");
        let defaulted = sqlx::query("INSERT INTO activitypub_actors (user_id) VALUES ($1)")
            .bind(user.id)
            .execute(&pool)
            .await
            .expect("a row without a status takes the default");
        assert_eq!(defaulted.rows_affected(), 1, "one row");
        let row = get(&pool, user.id).await.unwrap().expect("the row exists");
        assert_eq!(row.status, ActorStatus::Active, "the default is active");
    }

    #[tokio::test]
    async fn a_public_key_id_names_one_actor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = remote_user(&pool, "first").await;
        let second = remote_user(&pool, "second").await;
        let mut write = bare_write(first.id);
        write.public_key_id = Some("https://remote.example/users/first#main-key".to_owned());
        let _row = upsert(&pool, &write)
            .await
            .expect("the first key id stores");

        write.user_id = second.id;
        let err = upsert(&pool, &write)
            .await
            .expect_err("the unique index refuses a shared key id");
        assert!(matches!(err, AppError::Database(_)), "{err}");

        let _none = upsert(&pool, &bare_write(second.id))
            .await
            .expect("a row without a key id is no conflict");
    }

    /// `insert_new` writes a missing row and leaves an existing one alone.
    #[tokio::test]
    async fn insert_new_writes_once_and_reports_the_conflict() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let first = ActivityPubActorWrite {
            inbox: Some("https://test.example/first".to_owned()),
            ..bare_write(alice.id)
        };
        let second = ActivityPubActorWrite {
            inbox: Some("https://test.example/second".to_owned()),
            ..bare_write(alice.id)
        };

        let inserted = insert_new(&pool, &first).await.expect("the first insert");
        let repeated = insert_new(&pool, &second).await.expect("the second insert");

        assert!(inserted, "a missing row is inserted");
        assert!(!repeated, "an existing row is reported, not an error");
        let stored = get(&pool, alice.id).await.unwrap().expect("the row");
        assert_eq!(stored.inbox, first.inbox, "the first write is kept");
    }

    /// The constructor of a local account's claim: endpoints derived from
    /// the actor id and the instance, the generated keys, an active status.
    #[tokio::test]
    async fn a_local_claim_derives_the_endpoints_and_carries_the_keys() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let alice = local_user(db.pool(), "alice").await;
        let keys = KeyPair {
            private_pem: SECRET_KEY.to_owned(),
            public_pem: "-----BEGIN PUBLIC KEY-----alice".to_owned(),
        };

        let write = ActivityPubActorWrite::for_local_account(&alice, "https://test.example", keys);

        assert_eq!(write.user_id, alice.id, "the account's row");
        assert_eq!(write.status, ActorStatus::Active, "the identity is claimed");
        assert_eq!(write.inbox.as_deref(), Some(inbox(&alice.ap_id).as_str()));
        assert_eq!(
            write.shared_inbox.as_deref(),
            Some("https://test.example/inbox"),
            "the shared inbox is the instance's own"
        );
        assert_eq!(write.outbox.as_deref(), Some(outbox(&alice.ap_id).as_str()));
        assert_eq!(
            write.followers_url.as_deref(),
            Some(followers(&alice.ap_id).as_str())
        );
        assert_eq!(
            write.following_url.as_deref(),
            Some(following(&alice.ap_id).as_str())
        );
        assert_eq!(
            write.featured_url.as_deref(),
            Some(featured(&alice.ap_id).as_str())
        );
        assert_eq!(write.private_key.as_deref(), Some(SECRET_KEY));
        assert_eq!(
            write.public_key.as_deref(),
            Some("-----BEGIN PUBLIC KEY-----alice")
        );
        assert!(write.public_key_id.is_none(), "a local key id is implied");
        assert!(
            write.last_fetched_at.is_none(),
            "a local account is never fetched"
        );
    }
}

#[cfg(test)]
mod readers_tests;
