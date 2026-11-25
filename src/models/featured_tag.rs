//! The `featured_tags` table: the hashtags an account shows on its profile.
//!
//! Reference: Mastodon's `FeaturedTag` (`app/models/featured_tag.rb`): at
//! most [`MAX_PER_USER`] tags per account, one row per `(account, tag)`
//! pair, with the account's public usage of the tag counted beside it
//! (`statuses_count`, `last_status_at`). A row belongs to any account: a
//! local user writes theirs through the API ([`feature`], [`unfeature`]) and
//! a remote account's rows are the replacement of its `featuredTags`
//! collection ([`replace_all`]).
//!
//! The counts are recomputed, never incremented: [`refresh_conn`] counts the
//! account's live public and unlisted original statuses under each tag, and
//! runs in the transaction of every write that can change that number (a
//! status insert, an edit, a delete), so the stored counter cannot drift.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::hashtag::{self, Hashtag},
    util::id::new_id,
};

/// How many tags one account may feature: Mastodon's `LIMIT`.
pub const MAX_PER_USER: usize = 10;

/// How many tags [`suggestions`] offers: Mastodon's `Tag.suggestions` size.
pub const SUGGESTION_LIMIT: i64 = 10;

/// The statement behind [`refresh_conn`].
///
/// Every featured tag of the user (limited to the tags of one status when
/// `$2` is given) gets its count and last-use time recomputed from the live
/// public and unlisted originals the user wrote under it.
const REFRESH_SQL: &str = "UPDATE featured_tags ft
        SET statuses_count = (
              SELECT count(*) FROM status_hashtags sh
                JOIN statuses s ON s.id = sh.status_id
               WHERE sh.hashtag_id = ft.tag_id AND s.user_id = ft.user_id
                 AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL
                 AND s.visibility IN ('public', 'unlisted')),
            last_status_at = (
              SELECT max(s.inserted_at) FROM status_hashtags sh
                JOIN statuses s ON s.id = sh.status_id
               WHERE sh.hashtag_id = ft.tag_id AND s.user_id = ft.user_id
                 AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL
                 AND s.visibility IN ('public', 'unlisted'))
      WHERE ft.user_id = $1
        AND ($2::uuid IS NULL
             OR ft.tag_id IN (SELECT hashtag_id FROM status_hashtags WHERE status_id = $2))";

/// The lock [`feature`] and [`replace_all`] take first.
///
/// It is on the user's row, so two writes of one account's set run one after
/// the other and the limit cannot be raced past.
const LOCK_SQL: &str = "SELECT id FROM users WHERE id = $1 FOR UPDATE";

/// [`feature`]'s insert: the pair, left alone when it already exists.
const INSERT_SQL: &str = "INSERT INTO featured_tags (id, user_id, tag_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, tag_id) DO NOTHING";

/// The head of every [`Entry`] read: the featured row with its tag's name,
/// as a literal so [`concat!`] can splice it into each statement.
macro_rules! entry_select {
    () => {
        "SELECT ft.id, ft.user_id, ft.tag_id, h.name, ft.statuses_count,
       ft.last_status_at, ft.inserted_at
  FROM featured_tags ft
  JOIN hashtags h ON h.id = ft.tag_id"
    };
}

/// [`list`]: one user's entries by tag name.
const LIST_SQL: &str = concat!(entry_select!(), " WHERE ft.user_id = $1 ORDER BY h.name");

/// `find_conn`: one user's entry for one tag.
const FIND_SQL: &str = concat!(entry_select!(), " WHERE ft.user_id = $1 AND ft.tag_id = $2");

/// [`suggestions`]: the tags the user used most recently in their own
/// statuses that they do not feature yet, newest use first.
const SUGGESTIONS_SQL: &str = "SELECT h.id, h.name, h.inserted_at
  FROM hashtags h
  JOIN (SELECT sh.hashtag_id, max(s.inserted_at) AS last_used
          FROM statuses s
          JOIN status_hashtags sh ON sh.status_id = s.id
         WHERE s.user_id = $1 AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL
         GROUP BY sh.hashtag_id) used ON used.hashtag_id = h.id
 WHERE NOT EXISTS (SELECT 1 FROM featured_tags ft
                    WHERE ft.user_id = $1 AND ft.tag_id = h.id)
 ORDER BY used.last_used DESC, h.name
 LIMIT $2";

/// One featured tag: the row with its tag's name.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Entry {
    /// `UUIDv7` primary key of the `featured_tags` row: the entity's `id`.
    pub id: Uuid,
    /// The account that features the tag.
    pub user_id: Uuid,
    /// The featured tag's row id.
    pub tag_id: Uuid,
    /// The tag name as stored: lowercase, without the leading `#`.
    pub name: String,
    /// How many public and unlisted statuses of the account use the tag.
    pub statuses_count: i64,
    /// When the account last used the tag in such a status.
    pub last_status_at: Option<DateTime<Utc>>,
    /// When the tag was featured.
    pub inserted_at: DateTime<Utc>,
}

/// What [`feature`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Featured {
    /// The tag was not featured before: the row just written.
    Added(Entry),
    /// The account already featured the tag: its stored row, unchanged.
    Existing(Entry),
    /// The account already features [`MAX_PER_USER`] other tags.
    LimitReached,
}

impl Featured {
    /// The featured row, `None` when the limit refused the tag.
    #[must_use]
    pub fn entry(self) -> Option<Entry> {
        match self {
            Self::Added(entry) | Self::Existing(entry) => Some(entry),
            Self::LimitReached => None,
        }
    }
}

/// Features `tag` for `user_id`, with its current counts.
///
/// Idempotent on the pair: a tag the account already features answers its
/// stored row as [`Featured::Existing`]. A new tag on an account that already features
/// [`MAX_PER_USER`] others is refused as [`Featured::LimitReached`], and
/// nothing is written.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn feature(pool: &PgPool, user_id: Uuid, tag: &Hashtag) -> AppResult<Featured> {
    let mut tx = pool.begin().await?;
    sqlx::query(LOCK_SQL)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    if let Some(existing) = find_conn(&mut tx, user_id, tag.id).await? {
        return Ok(Featured::Existing(existing));
    }
    let held = count_conn(&mut tx, user_id).await?;
    if usize::try_from(held).unwrap_or(usize::MAX) >= MAX_PER_USER {
        return Ok(Featured::LimitReached);
    }
    insert_conn(&mut tx, user_id, tag.id).await?;
    refresh_conn(&mut tx, user_id, None).await?;
    let entry = find_conn(&mut tx, user_id, tag.id).await?;
    tx.commit().await?;
    Ok(entry.map_or(Featured::LimitReached, Featured::Added))
}

/// Replaces the whole featured set of `user_id` with the tags `names` name.
///
/// At most [`MAX_PER_USER`] tags are kept: what a remote actor's
/// `featuredTags` collection becomes locally. Unusable names are dropped and
/// repeats folded.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn replace_all(pool: &PgPool, user_id: Uuid, names: &[String]) -> AppResult<()> {
    let mut tags: Vec<Hashtag> = Vec::new();
    for name in names {
        if tags.len() >= MAX_PER_USER {
            break;
        }
        if let Some(tag) = hashtag::find_or_create(pool, name).await?
            && !tags.iter().any(|kept| kept.id == tag.id)
        {
            tags.push(tag);
        }
    }
    let ids: Vec<Uuid> = tags.iter().map(|tag| tag.id).collect();
    let mut tx = pool.begin().await?;
    sqlx::query(LOCK_SQL)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query("DELETE FROM featured_tags WHERE user_id = $1 AND NOT (tag_id = ANY($2))")
        .bind(user_id)
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    for id in &ids {
        insert_conn(&mut tx, user_id, *id).await?;
    }
    refresh_conn(&mut tx, user_id, None).await?;
    tx.commit().await?;
    Ok(())
}

/// Stops `user_id` featuring the row `id`; `true` when a row was removed.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn unfeature(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<bool> {
    let removed = sqlx::query("DELETE FROM featured_tags WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(removed > 0)
}

/// Stops `user_id` featuring the tag `tag_id`; `true` when a row was
/// removed.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn unfeature_tag(pool: &PgPool, user_id: Uuid, tag_id: Uuid) -> AppResult<bool> {
    let removed = sqlx::query("DELETE FROM featured_tags WHERE tag_id = $1 AND user_id = $2")
        .bind(tag_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(removed > 0)
}

/// The tags `user_id` features, by name.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Entry>> {
    let entries = sqlx::query_as::<_, Entry>(LIST_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(entries)
}

/// Whether `user_id` features the tag `tag_id`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn is_featuring(pool: &PgPool, user_id: Uuid, tag_id: Uuid) -> AppResult<bool> {
    let featuring = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM featured_tags WHERE user_id = $1 AND tag_id = $2)",
    )
    .bind(user_id)
    .bind(tag_id)
    .fetch_one(pool)
    .await?;
    Ok(featuring)
}

/// The tags `user_id` used lately and does not feature, newest use first.
///
/// At most [`SUGGESTION_LIMIT`] tags are returned.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn suggestions(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Hashtag>> {
    let tags = sqlx::query_as::<_, Hashtag>(SUGGESTIONS_SQL)
        .bind(user_id)
        .bind(SUGGESTION_LIMIT)
        .fetch_all(pool)
        .await?;
    Ok(tags)
}

/// Recomputes the counts of `user_id`'s featured tags on `conn`.
///
/// With `status_id` given, only the tags that status carries are refreshed.
/// Called inside the transaction of every write that can change a count. An
/// account that features nothing matches no row, so the common case is one
/// cheap indexed statement.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn refresh_conn(
    conn: &mut PgConnection,
    user_id: Uuid,
    status_id: Option<Uuid>,
) -> AppResult<()> {
    sqlx::query(REFRESH_SQL)
        .bind(user_id)
        .bind(status_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// The row of one `(user, tag)` pair, on `conn`.
async fn find_conn(
    conn: &mut PgConnection,
    user_id: Uuid,
    tag_id: Uuid,
) -> AppResult<Option<Entry>> {
    let entry = sqlx::query_as::<_, Entry>(FIND_SQL)
        .bind(user_id)
        .bind(tag_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(entry)
}

/// How many tags `user_id` features, on `conn`.
async fn count_conn(conn: &mut PgConnection, user_id: Uuid) -> AppResult<i64> {
    let held =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM featured_tags WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&mut *conn)
            .await?;
    Ok(held)
}

/// Inserts the pair on `conn`, leaving an existing one alone.
async fn insert_conn(conn: &mut PgConnection, user_id: Uuid, tag_id: Uuid) -> AppResult<()> {
    sqlx::query(INSERT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(tag_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, NewStatus, Visibility},
        test_fixtures::local_user,
    };

    /// The stored tag `name`, created when missing.
    async fn tag(pool: &PgPool, name: &str) -> Hashtag {
        hashtag::find_or_create(pool, name)
            .await
            .expect("the tag upserts")
            .expect("the name is usable")
    }

    /// Posts `text` as `author` with `visibility`.
    async fn post(
        pool: &PgPool,
        author: &crate::models::user::User,
        text: &str,
        visibility: Visibility,
    ) -> status::Status {
        let mut new_status = NewStatus::local(author, text);
        new_status.visibility = visibility;
        status::insert(pool, TEST_BASE_URL, &new_status)
            .await
            .expect("the fixture status inserts")
    }

    #[tokio::test]
    async fn featuring_is_idempotent_and_lists_by_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let zed = tag(&pool, "zed").await;
        let abc = tag(&pool, "abc").await;
        for wanted in [&zed, &abc] {
            let outcome = feature(&pool, alice.id, wanted).await.expect("it features");
            assert!(matches!(outcome, Featured::Added(_)), "{outcome:?}");
        }
        let repeat = feature(&pool, alice.id, &zed).await.expect("it features");
        assert!(matches!(repeat, Featured::Existing(_)), "{repeat:?}");
        let names: Vec<String> = list(&pool, alice.id)
            .await
            .expect("the list reads")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["abc", "zed"], "one row per tag, ordered by name");
        assert!(is_featuring(&pool, alice.id, zed.id).await.expect("reads"));
    }

    #[tokio::test]
    async fn the_eleventh_tag_is_refused_but_a_featured_one_is_not() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let mut rows: Vec<Hashtag> = Vec::new();
        for n in 0..MAX_PER_USER {
            let row = tag(&pool, &format!("tag{n}x")).await;
            let outcome = feature(&pool, alice.id, &row).await.expect("it features");
            assert!(matches!(outcome, Featured::Added(_)), "{outcome:?}");
            rows.push(row);
        }
        let extra = tag(&pool, "extra").await;
        assert_eq!(
            feature(&pool, alice.id, &extra).await.expect("it answers"),
            Featured::LimitReached,
            "the eleventh tag"
        );
        let again = rows.first().expect("a first tag");
        let outcome = feature(&pool, alice.id, again).await.expect("it answers");
        assert!(
            matches!(outcome, Featured::Existing(_)),
            "a tag already featured is no new one: {outcome:?}"
        );
        assert_eq!(
            list(&pool, alice.id).await.expect("reads").len(),
            MAX_PER_USER
        );
    }

    #[tokio::test]
    async fn counts_follow_public_posts_and_start_from_the_past() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let _old = post(&pool, &alice, "before #rust", Visibility::Public).await;
        let rust = tag(&pool, "rust").await;
        let entry = feature(&pool, alice.id, &rust)
            .await
            .expect("features")
            .entry()
            .expect("the first tag fits");
        assert_eq!(entry.statuses_count, 1, "the earlier post counts at once");
        assert!(entry.last_status_at.is_some(), "and dates the last use");

        let _private = post(&pool, &alice, "#rust hidden", Visibility::Private).await;
        let fresh = post(&pool, &alice, "#rust again", Visibility::Unlisted).await;
        let counted = list(&pool, alice.id).await.expect("reads");
        assert_eq!(
            counted[0].statuses_count, 2,
            "unlisted counts, private does not"
        );

        let removed = status::soft_delete(&pool, fresh.id).await.expect("deletes");
        assert!(removed.is_some(), "the post was live");
        let after_delete = list(&pool, alice.id).await.expect("reads");
        assert_eq!(
            after_delete[0].statuses_count, 1,
            "a deleted post is uncounted"
        );
    }

    #[tokio::test]
    async fn suggestions_are_recent_unfeatured_tags_of_the_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let _one = post(&pool, &alice, "#older", Visibility::Public).await;
        let _two = post(&pool, &alice, "#newer #shown", Visibility::Public).await;
        let _other = post(&pool, &bob, "#bobs", Visibility::Public).await;
        let shown = tag(&pool, "shown").await;
        let _featured = feature(&pool, alice.id, &shown).await.expect("features");
        let names: Vec<String> = suggestions(&pool, alice.id)
            .await
            .expect("reads")
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["newer", "older"], "own, recent, not yet featured");
    }

    #[tokio::test]
    async fn replacing_the_set_keeps_ten_and_drops_the_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let remote = local_user(&pool, "remote").await;
        let first: Vec<String> = ["Alpha", "#beta", "alpha", "no way", "gamma"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        replace_all(&pool, remote.id, &first)
            .await
            .expect("replaces");
        let names = |rows: Vec<Entry>| rows.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(
            names(list(&pool, remote.id).await.expect("reads")),
            ["alpha", "beta", "gamma"],
            "usable, folded names"
        );
        let many: Vec<String> = (0..15).map(|n| format!("t{n}x")).collect();
        replace_all(&pool, remote.id, &many)
            .await
            .expect("replaces");
        assert_eq!(
            list(&pool, remote.id).await.expect("reads").len(),
            MAX_PER_USER,
            "the set is capped"
        );
        replace_all(&pool, remote.id, &[]).await.expect("clears");
        assert_eq!(
            list(&pool, remote.id).await.expect("reads"),
            Vec::<Entry>::new(),
            "an empty collection clears the set"
        );
    }

    #[tokio::test]
    async fn unfeaturing_removes_one_row_and_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let rust = tag(&pool, "rust").await;
        let entry = feature(&pool, alice.id, &rust)
            .await
            .expect("features")
            .entry()
            .expect("fits");
        assert!(
            !unfeature(&pool, bob.id, entry.id).await.expect("runs"),
            "another account cannot remove it"
        );
        assert!(unfeature(&pool, alice.id, entry.id).await.expect("runs"));
        assert!(!unfeature(&pool, alice.id, entry.id).await.expect("runs"));
        let _again = feature(&pool, alice.id, &rust).await.expect("features");
        assert!(unfeature_tag(&pool, alice.id, rust.id).await.expect("runs"));
        assert!(!unfeature_tag(&pool, alice.id, rust.id).await.expect("runs"));
    }
}
