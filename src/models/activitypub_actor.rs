//! The `activitypub_actors` table: `ActivityPub`'s own data about an account.
//!
//! One row per account (local or remote) that exists on `ActivityPub`. This is `ActivityPub`'s own table under the `<protocol id>_...` naming
//! rule (WBE-832): `users` holds what every protocol shares, and each
//! protocol keeps the rest in tables named after it. The row holds the
//! actor's endpoints and collection ids, its signing key pair, the key id
//! its document names and when it was last fetched. `ap_id`, `domain`,
//! `actor_type` and the other columns client APIs render on every account
//! stay on `users`.
//!
//! [`ActivityPubActor::status`] says whether the account's `ActivityPub`
//! identity is claimed ([`ActorStatus::Active`]) or awaiting setup
//! ([`ActorStatus::Pending`]).
//!
//! Nothing reads this table outside its own tests yet. The migration
//! backfilled it from `users`, and every writer of the moved columns now
//! writes both tables in one transaction ([`upsert_from_user`] is how):
//! `models::user::insert` for every new local or remote account, and
//! `federation::remote_actor::upsert_remote_user` for every fetch of a
//! remote actor.
//!
//! **The dual write is temporary.** It keeps the two tables in step while
//! the readers move over; the ticket that drops the moved columns from
//! `users` removes it, and the callers of [`upsert_from_user`] then write
//! [`ActivityPubActorWrite`] directly. Deleting the user deletes the row
//! (the foreign key cascades).
//!
//! [`get`] and [`get_many`] read, [`get_many`] taking every id in one
//! statement so a caller holding a list never queries in a loop, and
//! [`upsert`] writes the whole row: it inserts, or on a conflict of
//! `user_id` overwrites every column and moves `updated_at`.

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{Executor, Postgres};
use uuid::Uuid;

use crate::{error::AppResult, models::user::User};

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

/// Mirrors the moved columns of `user` into the account's row.
///
/// Returns the row as stored. This is the temporary dual write: a writer of `users` calls it on the same
/// transaction right after its own statement, passing the row that
/// statement returned, so both tables hold the same values. `public_key_id`
/// is not a field of [`User`], so the writer that sets it passes the value
/// it stored. The status is [`ActorStatus::Active`], the column default,
/// since `users` has no such notion. The ticket that drops the old `users`
/// columns removes this function.
///
/// # Errors
///
/// As [`upsert`].
pub async fn upsert_from_user<'e, E>(
    executor: E,
    user: &User,
    public_key_id: Option<&str>,
) -> AppResult<ActivityPubActor>
where
    E: Executor<'e, Database = Postgres>,
{
    let write = ActivityPubActorWrite {
        user_id: user.id,
        status: ActorStatus::Active,
        inbox: user.inbox.clone(),
        shared_inbox: user.shared_inbox.clone(),
        outbox: user.outbox.clone(),
        followers_url: user.followers_url.clone(),
        following_url: user.following_url.clone(),
        featured_url: user.featured_url.clone(),
        public_key: user.public_key.clone(),
        private_key: user.private_key.clone(),
        public_key_id: public_key_id.map(str::to_owned),
        last_fetched_at: user.last_fetched_at,
    };
    upsert(executor, &write).await
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        error::AppError,
        test_fixtures::{local_user, remote_new, remote_user},
    };

    /// Removes the row every insert of a fixture user wrote, for a test that
    /// needs an account with no row (or that writes the row itself).
    async fn drop_rows(pool: &PgPool) {
        let removed = sqlx::query("DELETE FROM activitypub_actors")
            .execute(pool)
            .await
            .expect("the rows delete")
            .rows_affected();
        assert!(removed > 0, "the fixture users came with rows to remove");
    }

    /// The secret the redaction tests look for.
    const SECRET_KEY: &str = "-----BEGIN PRIVATE KEY-----secret";

    /// The backfill statement of the `activitypub_actors` migration, verbatim.
    ///
    /// The migration ran when the database was built, before any test user
    /// existed, so a test that wants the backfill's effect runs it again.
    const BACKFILL_SQL: &str = "INSERT INTO activitypub_actors (
    user_id, inbox, shared_inbox, outbox, followers_url, following_url,
    featured_url, public_key, private_key, public_key_id, last_fetched_at
)
SELECT
    id, inbox, shared_inbox, outbox, followers_url, following_url,
    featured_url, public_key, private_key, public_key_id, last_fetched_at
FROM users";

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
        drop_rows(&pool).await;
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
        drop_rows(&pool).await;

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
        drop_rows(&pool).await;
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
        drop_rows(&pool).await;

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

    #[tokio::test]
    async fn the_backfill_copies_every_user_local_and_remote() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut remote = remote_new("far");
        remote.inbox = Some("https://remote.example/users/far/inbox".to_owned());
        remote.shared_inbox = Some("https://remote.example/inbox".to_owned());
        remote.outbox = Some("https://remote.example/users/far/outbox".to_owned());
        remote.followers_url = Some("https://remote.example/users/far/followers".to_owned());
        remote.following_url = Some("https://remote.example/users/far/following".to_owned());
        remote.featured_url = Some("https://remote.example/users/far/featured".to_owned());
        remote.public_key = Some("-----BEGIN PUBLIC KEY-----far".to_owned());
        remote.last_fetched_at = DateTime::from_timestamp(1_700_000_000, 0);
        let far = crate::models::user::insert(&pool, &remote)
            .await
            .expect("the remote user inserts");
        let keyed = sqlx::query("UPDATE users SET public_key_id = $2 WHERE id = $1")
            .bind(far.id)
            .bind("https://remote.example/users/far#main-key")
            .execute(&pool)
            .await
            .expect("the key id is set")
            .rows_affected();
        assert_eq!(keyed, 1, "the key id went onto the one user");
        let mut local = crate::test_fixtures::local_new("alice");
        local.private_key = Some(SECRET_KEY.to_owned());
        local.public_key = Some("-----BEGIN PUBLIC KEY-----alice".to_owned());
        let alice = crate::models::user::insert(&pool, &local)
            .await
            .expect("the local user inserts");
        drop_rows(&pool).await;
        assert_eq!(row_count(&pool).await, 0, "the table starts empty");

        let copied = sqlx::query(BACKFILL_SQL)
            .execute(&pool)
            .await
            .expect("the backfill runs")
            .rows_affected();

        assert_eq!(copied, 2, "one row per user");
        let far_row = get(&pool, far.id).await.unwrap().expect("the remote row");
        assert_eq!(
            far_row.status,
            ActorStatus::Active,
            "backfilled rows are active"
        );
        assert_eq!(far_row.inbox, remote.inbox);
        assert_eq!(far_row.shared_inbox, remote.shared_inbox);
        assert_eq!(far_row.outbox, remote.outbox);
        assert_eq!(far_row.followers_url, remote.followers_url);
        assert_eq!(far_row.following_url, remote.following_url);
        assert_eq!(far_row.featured_url, remote.featured_url);
        assert_eq!(far_row.public_key, remote.public_key);
        assert_eq!(
            far_row.public_key_id.as_deref(),
            Some("https://remote.example/users/far#main-key")
        );
        assert_eq!(far_row.last_fetched_at, remote.last_fetched_at);
        let alice_row = get(&pool, alice.id).await.unwrap().expect("the local row");
        assert_eq!(
            alice_row.private_key, local.private_key,
            "the local key pair came too"
        );
        assert_eq!(alice_row.public_key, local.public_key);
        assert!(
            alice_row.last_fetched_at.is_none(),
            "a local account was never fetched"
        );
    }
}
