//! The `hashtags` and `status_hashtags` tables: the tags a status carries.
//!
//! Reference: Pleroma normalizes tag names and keeps one `hashtags` row per
//! name behind an object join table (`lib/pleroma/hashtag.ex`). Here the
//! name is stored lowercase and without the leading `#` (the same shape
//! [`crate::util::entities::extract_hashtags`] yields), so the unique key is
//! plain text and the entity renders straight from storage; the reverse
//! lookup — the newest statuses under one tag — is an index seek on
//! `(hashtag_id, status_id DESC)`.
//!
//! Unlike [`crate::models::mention`] this edge carries no position: the
//! Status entity's `tags` array orders by name, so
//! [`for_statuses`] sorts on the column and [`link_to_status`] only has to
//! keep pairs unique. [`upsert_many`] is the idempotent, case-insensitive
//! write: re-using a tag points at the row that already holds it, and names
//! that are blank or, once normalized, longer than the 100-character
//! extraction bound are ignored rather than stored.
//!
//! Nothing here extracts tags from text: [`crate::models::status::insert`]
//! upserts and links what its caller computed —
//! [`crate::util::entities::extract_hashtags`] of the raw text for a local
//! status, the `tag` list of the activity for a remote one — in the
//! status's own transaction.
//!
//! [`day_counts`] is the read behind the tag endpoints' `history`: the one
//! tag's live statuses, bucketed by UTC day.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::status::Status,
    util::{entities::MAX_HASHTAG_LEN, id::new_id},
};

/// The statement behind [`upsert_many`]: one row per given `(id, name)`
/// pair, and on a name that already exists the no-op update of
/// `name = EXCLUDED.name` — the shape the ticket prescribes, and what makes
/// `RETURNING` answer with the stored row (its original `id` and
/// `inserted_at`) instead of raising the unique violation.
const UPSERT_SQL: &str = "INSERT INTO hashtags (id, name)
     SELECT u.id, u.name FROM unnest($1::uuid[], $2::text[]) AS u(id, name)
ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
RETURNING id, name, inserted_at";

/// The statement behind [`link_to_status`]: one pair per id, and a pair the
/// status already has is skipped, so linking twice changes nothing.
const LINK_SQL: &str = "INSERT INTO status_hashtags (status_id, hashtag_id)
     SELECT $1::uuid, t.hashtag_id FROM unnest($2::uuid[]) AS t(hashtag_id)
ON CONFLICT (status_id, hashtag_id) DO NOTHING";

/// The statement behind [`replace_for_status`]: the old set goes first, and
/// the freshly upserted names are linked after it, in the same transaction.
const DELETE_FOR_STATUS_SQL: &str = "DELETE FROM status_hashtags WHERE status_id = $1";

/// The leading text of [`for_statuses`]: every tag of the given statuses,
/// each row led by the status it tags, ordered status first and then by
/// tag name — the array order the entity renders.
const FOR_STATUSES_SQL: &str = "SELECT sh.status_id, h.* FROM status_hashtags sh
      JOIN hashtags h ON h.id = sh.hashtag_id
     WHERE sh.status_id = ANY($1)
     ORDER BY sh.status_id, h.name";

/// [`get_by_name`]: one row by its stored (normalized) name.
const BY_NAME_SQL: &str = "SELECT * FROM hashtags WHERE name = $1";

/// The statement behind [`day_counts`]: the tag's live statuses bucketed by
/// UTC calendar day. `AT TIME ZONE 'UTC'` makes the day boundary explicit
/// (the session zone must not move a status between two days), soft-deleted
/// rows are out (their content is gone, so their use of the tag is), and
/// `COUNT(DISTINCT user_id)` is the day's account count — Mastodon's
/// `uses`/`accounts` pair.
const DAY_COUNTS_SQL: &str = "SELECT (s.inserted_at AT TIME ZONE 'UTC')::date,
       COUNT(*)::bigint, COUNT(DISTINCT s.user_id)::bigint
  FROM status_hashtags sh
  JOIN statuses s ON s.id = sh.status_id
       WHERE sh.hashtag_id = $1 AND s.deleted_at IS NULL AND s.inserted_at >= $2
       GROUP BY 1
       ORDER BY 1";

/// [`public_posts`]: the live `public` root posts carrying one tag, newest
/// id first over the `(hashtag_id, status_id DESC)` index, optionally a
/// keyset page older than the cursor. Replies and reblogs are out: the tag
/// feed advertises what people said, not their boosts or thread replies.
const PUBLIC_POSTS_SQL: &str = "SELECT s.* FROM statuses s
   JOIN status_hashtags sh ON sh.status_id = s.id
  WHERE sh.hashtag_id = $1
    AND s.visibility = 'public'
    AND s.deleted_at IS NULL
    AND s.in_reply_to_id IS NULL
    AND s.reblog_of_id IS NULL
    AND ($2::uuid IS NULL OR s.id < $2)
ORDER BY s.id DESC
    LIMIT $3";

/// One row of `hashtags`: one tag name, shared by every status that uses it.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Hashtag {
    /// `UUIDv7` primary key, minted by [`upsert_many`] (decision 4).
    pub id: Uuid,
    /// The tag name as stored: lowercase, without the leading `#`.
    pub name: String,
    /// When this tag was first created.
    pub inserted_at: DateTime<Utc>,
}

/// The joined row of [`for_statuses`]: one tag link with the [`Hashtag`] its
/// `hashtag_id` names, flattened.
#[derive(Debug, sqlx::FromRow)]
struct StatusHashtagRow {
    /// The status this row links, from `status_hashtags.status_id`.
    status_id: Uuid,
    /// The tag, from the joined `hashtags` row.
    #[sqlx(flatten)]
    hashtag: Hashtag,
}

/// Upserts every usable name in `names` and returns the rows they name,
/// ordered by name.
///
/// A name is normalized (trimmed, `#`-stripped, lowercased) before it is
/// looked at, so `#Foo`, `Foo` and `foo` are one tag; a name that
/// normalizes to nothing or to more than 100 characters (the extraction
/// bound, [`crate::util::entities::extract_hashtags`]) is ignored.
/// Idempotent: a name the table already holds answers its stored row, and
/// repeats within one call are folded to one write.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn upsert_many(pool: &PgPool, names: &[String]) -> AppResult<Vec<Hashtag>> {
    let mut conn = pool.acquire().await?;
    upsert_many_conn(&mut conn, names).await
}

/// [`upsert_many`] on an open connection: the statement runs on the
/// caller's transaction, which is how [`crate::models::status::insert`]
/// writes a status and its tags together.
///
/// An empty batch short-circuits: no statement, no round trip.
pub(crate) async fn upsert_many_conn(
    conn: &mut PgConnection,
    names: &[String],
) -> AppResult<Vec<Hashtag>> {
    let names = normalized_names(names);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = names.iter().map(|_| new_id()).collect();
    let mut hashtags = sqlx::query_as::<_, Hashtag>(UPSERT_SQL)
        .bind(&ids)
        .bind(&names)
        .fetch_all(&mut *conn)
        .await?;
    // The rows come back in whatever order the statement folded them; the
    // name order is what callers (and tests) get to rely on.
    hashtags.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(hashtags)
}

/// Finds or creates the one tag `name` names.
///
/// The single-name face of [`upsert_many`]: `#Foo` finds `foo`, and an
/// unknown usable name gets a row. A name that normalization rejects
/// (blank, or longer than the 100-character stored-name bound) is `None` —
/// there is no tag to find and none to store.
///
/// # Errors
///
/// Any database failure behind the upsert (connection, encoding) arrives
/// as [`AppError::Database`](crate::error::AppError).
pub async fn find_or_create(pool: &PgPool, name: &str) -> AppResult<Option<Hashtag>> {
    let mut usable: Vec<String> = Vec::new();
    if let Some(tag) = normalize_name(name) {
        usable.push(tag);
    }
    let mut found = upsert_many(pool, &usable).await?;
    Ok(found.pop())
}

/// The stored tag whose name normalizes to `name`'s, `None` when no row
/// holds it — including when `name` is not a usable tag at all.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_name(pool: &PgPool, name: &str) -> AppResult<Option<Hashtag>> {
    let Some(tag) = normalize_name(name) else {
        return Ok(None);
    };
    let hashtag = sqlx::query_as::<_, Hashtag>(BY_NAME_SQL)
        .bind(&tag)
        .fetch_optional(pool)
        .await?;
    Ok(hashtag)
}

/// The per-UTC-day usage of tag `hashtag_id`, oldest bucket first, for every
/// live status created at or after `since`.
///
/// The `(day, uses, accounts)` triples are what the tag endpoints'
/// `history` is built from; a day no status used the tag on is simply
/// absent — the caller owns the calendar and renders the gap as zeros.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn day_counts(
    pool: &PgPool,
    hashtag_id: Uuid,
    since: DateTime<Utc>,
) -> AppResult<Vec<(NaiveDate, i64, i64)>> {
    let rows = sqlx::query_as::<_, (NaiveDate, i64, i64)>(DAY_COUNTS_SQL)
        .bind(hashtag_id)
        .bind(since)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The newest `limit` live `public` root posts carrying tag `hashtag_id`,
/// newest first (decision 4): the page the tag's `Atom`/`RSS` feeds
/// ([`crate::api::feeds`]) enumerate.
///
/// This is the reverse lookup the `status_hashtags_hashtag_id_idx` index
/// exists to serve. As in the account feed, replies and reblogs are out and
/// only `public` visibility counts; `max_id`, when given, keys the page on
/// the status id.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn public_posts(
    pool: &PgPool,
    hashtag_id: Uuid,
    max_id: Option<Uuid>,
    limit: i64,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(PUBLIC_POSTS_SQL)
        .bind(hashtag_id)
        .bind(max_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// Links the tags in `hashtag_ids` to status `status_id`.
///
/// Pairs the status already has are skipped, so linking twice changes
/// nothing, and an id repeated within one call is folded by the key. Order
/// is not kept: the entity renders [`for_statuses`]' name order.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown status or tag
/// arrives as the foreign-key violation the database reports, unless the
/// call links nothing at all (an empty `hashtag_ids` touches no row and
/// checks no key).
pub async fn link_to_status(pool: &PgPool, status_id: Uuid, hashtag_ids: &[Uuid]) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    link_conn(&mut conn, status_id, hashtag_ids).await
}

/// [`link_to_status`] on an open connection: the statement runs on the
/// caller's transaction.
///
/// An empty batch short-circuits: no statement, no round trip, and no
/// foreign-key check either (nothing is written, so an unknown `status_id`
/// with nothing to link is no error).
pub(crate) async fn link_conn(
    conn: &mut PgConnection,
    status_id: Uuid,
    hashtag_ids: &[Uuid],
) -> AppResult<()> {
    if hashtag_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(LINK_SQL)
        .bind(status_id)
        .bind(hashtag_ids)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Upserts `names` and links every tag they name to status `status_id`, on
/// `conn`: the pair of writes [`crate::models::status::insert`] runs inside
/// the status's transaction.
pub(crate) async fn link_names_conn(
    conn: &mut PgConnection,
    status_id: Uuid,
    names: &[String],
) -> AppResult<()> {
    let hashtags = upsert_many_conn(conn, names).await?;
    let ids: Vec<Uuid> = hashtags.iter().map(|hashtag| hashtag.id).collect();
    link_conn(conn, status_id, &ids).await
}

/// Replaces the whole tag set of status `status_id` with `names`, in one
/// transaction.
///
/// This is the shape an edit of a post needs: every link the status has is
/// deleted and the upserted names of `names` are linked in their place.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown `status_id`
/// with at least one usable name is the foreign-key violation the database
/// reports, and the transaction rolls every part of the replacement back
/// with it.
pub async fn replace_for_status(pool: &PgPool, status_id: Uuid, names: &[String]) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(DELETE_FOR_STATUS_SQL)
        .bind(status_id)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    link_names_conn(&mut tx, status_id, names).await?;
    tx.commit().await?;
    Ok(())
}

/// The tags of every status in `ids`, keyed by status id, each entry
/// ordered by name.
///
/// Statuses the caller asked about that carry no tags are absent from the
/// map, and an empty `ids` answers the empty map; this is the batch read
/// that renders a page of statuses without one query per row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_statuses(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, Vec<Hashtag>>> {
    let ids = ids.to_vec();
    let rows = sqlx::query_as::<_, StatusHashtagRow>(FOR_STATUSES_SQL)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    let mut grouped: HashMap<Uuid, Vec<Hashtag>> = HashMap::new();
    for row in rows {
        grouped.entry(row.status_id).or_default().push(row.hashtag);
    }
    Ok(grouped)
}

/// The normalized form of one written tag name, or `None` when it is not a
/// usable tag: trimmed, leading `#`s dropped, lowercased — and non-empty
/// and at most [`MAX_HASHTAG_LEN`] characters once normalized.
///
/// The tag endpoints ([`crate::api::tags`]) run the path segment through
/// this before storing or rendering anything: `None` is their
/// "Invalid hashtag" case, the lowercased text the entity's name.
pub(crate) fn normalize_name(name: &str) -> Option<String> {
    let trimmed = name.trim().trim_start_matches('#').trim();
    if trimmed.is_empty() {
        return None;
    }
    let tag = trimmed.to_lowercase();
    // The length bound is on the stored tag, so it is checked after the
    // lowercase (which can change a character count at the margins).
    if tag.chars().count() > MAX_HASHTAG_LEN {
        return None;
    }
    Some(tag)
}

/// `names` normalized, unusable names dropped and repeats folded to their
/// first occurrence, in order: what [`UPSERT_SQL`] inserts, so one call
/// never offers the same name twice (which `ON CONFLICT DO UPDATE` refuses
/// to fold inside a single statement).
fn normalized_names(names: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut usable: Vec<String> = Vec::new();
    for name in names {
        let Some(tag) = normalize_name(name) else {
            continue;
        };
        if seen.insert(tag.clone()) {
            usable.push(tag);
        }
    }
    usable
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            status::{self, NewStatus, Visibility},
            user::{NewUser, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;

    /// The names of `hashtags`, in the order they came back.
    fn names(hashtags: &[Hashtag]) -> Vec<String> {
        hashtags
            .iter()
            .map(|hashtag| hashtag.name.clone())
            .collect()
    }

    /// The stored tag rows as `(name, id)`, ordered by name, read straight
    /// from storage so assertions do not depend on fixture return values.
    async fn stored_tags(pool: &PgPool) -> Vec<(String, Uuid)> {
        sqlx::query_as::<_, (String, Uuid)>("SELECT name, id FROM hashtags ORDER BY name")
            .fetch_all(pool)
            .await
            .expect("the stored tags read")
    }

    /// The stored links of status `status_id` as tag names, ordered by name.
    async fn stored_links(pool: &PgPool, status_id: Uuid) -> Vec<String> {
        sqlx::query_as::<_, (String,)>(
            "SELECT h.name FROM status_hashtags sh
               JOIN hashtags h ON h.id = sh.hashtag_id
              WHERE sh.status_id = $1
              ORDER BY h.name",
        )
        .bind(status_id)
        .fetch_all(pool)
        .await
        .expect("the stored links read")
        .into_iter()
        .map(|row| row.0)
        .collect()
    }

    /// The SQLSTATE a database failure carries.
    fn sqlstate_of(err: AppError) -> String {
        if let AppError::Database(SqlxError::Database(db_err)) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    /// A `Vec<String>` of the given written names, for the &[String] APIs.
    fn tags(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_case_insensitive() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = upsert_many(&pool, &tags(&["Foo"]))
            .await
            .expect("the first upsert stores");
        assert_eq!(names(&first), vec!["foo"], "the name is stored lowercase");

        let again = upsert_many(&pool, &tags(&["foo", "#FOO", " #Foo ", "FOO"]))
            .await
            .expect("a repeat upsert runs");
        assert_eq!(
            again, first,
            "every spelling answers the one stored row, folds included"
        );
        assert_eq!(
            stored_tags(&pool).await,
            vec![("foo".to_owned(), first[0].id)],
            "and the table still holds exactly one tag"
        );
    }

    #[tokio::test]
    async fn upsert_keeps_order_by_name_and_drops_the_unusable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let long = "t".repeat(MAX_HASHTAG_LEN + 1);
        let stored = upsert_many(
            &pool,
            &tags(&["beta", &long, "", "#", "  ", "ALPHA", "beta"]),
        )
        .await
        .expect("a mixed batch runs");
        assert_eq!(
            names(&stored),
            vec!["alpha", "beta"],
            "results come ordered by name"
        );
        assert_eq!(
            stored_tags(&pool).await.len(),
            2,
            "the over-long, blank and hash-only names were ignored"
        );

        let exactly_max = "x".repeat(MAX_HASHTAG_LEN);
        let kept = upsert_many(&pool, &tags(&[&exactly_max]))
            .await
            .expect("a 100-character name is usable");
        assert_eq!(
            names(&kept),
            vec![exactly_max.as_str()],
            "the bound itself is kept, only longer is ignored"
        );
    }

    #[tokio::test]
    async fn find_or_create_and_get_by_name_agree() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            get_by_name(&pool, "rust").await.unwrap().is_none(),
            "an unknown name is no tag yet"
        );
        let created = find_or_create(&pool, "#Rust")
            .await
            .expect("creating works")
            .expect("the name is usable");
        assert_eq!(created.name, "rust", "the row is the normalized tag");

        let found = find_or_create(&pool, "rust")
            .await
            .expect("finding works")
            .expect("the name is usable");
        assert_eq!(found, created, "and answers the very same row");
        assert_eq!(
            get_by_name(&pool, "RUST")
                .await
                .unwrap()
                .expect("get_by_name normalizes too"),
            created,
            "any spelling of the name finds it"
        );
        assert!(
            find_or_create(&pool, "   ").await.unwrap().is_none(),
            "a blank name is nothing to find or create"
        );
        assert!(
            get_by_name(&pool, &"t".repeat(MAX_HASHTAG_LEN + 1))
                .await
                .unwrap()
                .is_none(),
            "an over-long name is nothing to find"
        );
    }

    /// A local user named `nickname` and one public status of theirs.
    async fn post(pool: &PgPool, nickname: &str, text: &str) -> Status {
        let author = insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("fixture user inserts");
        status::insert(pool, BASE, &NewStatus::local(&author, text))
            .await
            .expect("fixture status inserts")
    }

    #[tokio::test]
    async fn link_to_status_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status = post(&pool, "alice", "hello").await;
        let foo = find_or_create(&pool, "#foo").await.unwrap().expect("foo");
        let bar = find_or_create(&pool, "bar").await.unwrap().expect("bar");

        link_to_status(&pool, status.id, &[foo.id, bar.id, foo.id])
            .await
            .expect("the first link stores");
        link_to_status(&pool, status.id, &[bar.id])
            .await
            .expect("a repeat link changes nothing");
        assert_eq!(
            stored_links(&pool, status.id).await,
            vec!["bar", "foo"],
            "one row per pair, the doubled id folded"
        );

        link_to_status(&pool, status.id, &[])
            .await
            .expect("linking nothing is no work");
    }

    #[tokio::test]
    async fn an_unknown_side_of_a_link_is_the_foreign_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status = post(&pool, "alice", "hello").await;
        let err = link_to_status(&pool, status.id, &[Uuid::now_v7()])
            .await
            .expect_err("the foreign key refuses an unknown tag");
        assert_eq!(sqlstate_of(err), "23503", "a missing tag is an FK");
        assert!(
            stored_links(&pool, status.id).await.is_empty(),
            "nothing stored"
        );

        let status_err = link_to_status(&pool, Uuid::now_v7(), &[status.id])
            .await
            .expect_err("and so is an unknown status");
        assert_eq!(
            sqlstate_of(status_err),
            "23503",
            "the other side is checked too"
        );
    }

    #[tokio::test]
    async fn replace_rewrites_the_whole_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status = post(&pool, "alice", "#old #shared").await;
        assert_eq!(
            stored_links(&pool, status.id).await,
            vec!["old", "shared"],
            "the fixture came tagged"
        );

        replace_for_status(&pool, status.id, &tags(&["new", "SHARED"]))
            .await
            .expect("the replacement runs");
        assert_eq!(
            stored_links(&pool, status.id).await,
            vec!["new", "shared"],
            "the old set is gone, the shared tag kept its row"
        );
        assert_eq!(
            stored_tags(&pool).await.len(),
            3,
            "the abandoned `old` tag row itself stays"
        );

        replace_for_status(&pool, status.id, &[])
            .await
            .expect("replacing with nothing runs");
        assert!(
            stored_links(&pool, status.id).await.is_empty(),
            "and an empty replacement clears the status"
        );
    }

    #[tokio::test]
    async fn for_statuses_groups_by_status_in_name_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let tagged = post(&pool, "alice", "#beta #alpha #beta").await;
        let shared = post(&pool, "bob", "#alpha").await;
        let quiet = post(&pool, "carol", "no tags").await;

        let map = for_statuses(&pool, &[tagged.id, shared.id, quiet.id, Uuid::now_v7()])
            .await
            .expect("the batch read works");
        assert_eq!(
            names(map.get(&tagged.id).expect("the tagged status")),
            vec!["alpha", "beta"],
            "each entry is ordered by name, repeats folded"
        );
        assert_eq!(
            names(map.get(&shared.id).expect("the shared status")),
            vec!["alpha"],
            "and a shared tag is the same row under both statuses"
        );
        assert_eq!(
            map.get(&shared.id).expect("shared")[0].id,
            map.get(&tagged.id).expect("tagged")[0].id,
            "one `alpha` for both"
        );
        assert!(
            !map.contains_key(&quiet.id),
            "a status without tags is absent, not an empty entry"
        );
        assert_eq!(map.len(), 2, "and an unknown id is absent as well");
        assert!(
            for_statuses(&pool, &[]).await.unwrap().is_empty(),
            "asking about nothing answers nothing"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_cascades_its_links() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let one = post(&pool, "alice", "#shared #mine").await;
        let two = post(&pool, "bob", "#shared").await;

        let gone = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(one.id)
            .execute(&pool)
            .await
            .expect("the fixture status hard-deletes");
        assert_eq!(gone.rows_affected(), 1, "one status went");
        assert!(
            stored_links(&pool, one.id).await.is_empty(),
            "its links cascaded with it"
        );
        assert_eq!(
            stored_links(&pool, two.id).await,
            vec!["shared"],
            "the other status keeps its link"
        );
        assert_eq!(
            stored_tags(&pool).await.len(),
            2,
            "and the tag rows stay for reuse"
        );
    }

    /// The `(day, uses, accounts)` buckets of tag `name` over the last week,
    /// read straight from [`day_counts`] with a window anchored on `anchor`.
    async fn week_of(
        pool: &PgPool,
        name: &str,
        anchor: DateTime<Utc>,
    ) -> Vec<(NaiveDate, i64, i64)> {
        let hashtag = get_by_name(pool, name)
            .await
            .expect("the tag reads")
            .expect("the fixture tag exists");
        let since = noon(anchor, 6);
        day_counts(pool, hashtag.id, since)
            .await
            .expect("the buckets read")
    }

    /// Noon UTC of the day `days_ago` before `anchor`'s date: the fixture
    /// timestamp one status's bucket day names. Noon (never a boundary) and
    /// one anchor for every fixture mean a UTC midnight crossing during the
    /// run cannot move a bucket or shift an expectation — each fixture's
    /// bucket day is pinned to the date it was computed from.
    fn noon(anchor: DateTime<Utc>, days_ago: i64) -> DateTime<Utc> {
        anchor
            .date_naive()
            .checked_sub_days(chrono::Days::new(
                u64::try_from(days_ago).expect("in range"),
            ))
            .expect("the fixture day exists")
            .and_hms_opt(12, 0, 0)
            .expect("noon")
            .and_utc()
    }

    /// Pin status `id`'s `inserted_at` to [`noon(anchor, days_ago)`].
    async fn pin_day(pool: &PgPool, id: Uuid, anchor: DateTime<Utc>, days_ago: i64) {
        let pinned = sqlx::query("UPDATE statuses SET inserted_at = $2 WHERE id = $1")
            .bind(id)
            .bind(noon(anchor, days_ago))
            .execute(pool)
            .await
            .expect("the pin runs");
        assert_eq!(pinned.rows_affected(), 1, "the fixture status moved");
    }

    #[tokio::test]
    async fn day_counts_buckets_per_utc_day_counting_uses_and_authors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let anchor = Utc::now();
        let today = anchor.date_naive();
        let alice = post(&pool, "alice", "#rust twice #rust").await;
        let bob = post(&pool, "bob", "#rust").await;
        let python = post(&pool, "carol", "#python").await;
        let aged = post(&pool, "dave", "#rust").await;
        let gone = post(&pool, "erin", "#rust").await;
        for (status, days_ago) in [(&alice, 0), (&bob, 0), (&python, 0), (&aged, 2), (&gone, 0)] {
            pin_day(&pool, status.id, anchor, days_ago).await;
        }
        let tombstone = status::soft_delete(&pool, gone.id)
            .await
            .expect("the fixture status soft-deletes");
        assert!(
            tombstone.expect("the status was live").deleted_at.is_some(),
            "and it is a tombstone now"
        );

        let buckets = week_of(&pool, "rust", anchor).await;
        assert_eq!(
            buckets,
            vec![(today - Duration::days(2), 1, 1), (today, 2, 2)],
            "one bucket per used day, oldest first: `twice #rust` is one use, \
             the deleted status is no use at all"
        );
        assert_eq!(
            week_of(&pool, "python", anchor).await,
            vec![(today, 1, 1)],
            "the other tag of the same day keeps its own buckets — the read is \
             per tag id"
        );
        let untouched = find_or_create(&pool, "untouched")
            .await
            .unwrap()
            .expect("the name is usable");
        assert!(
            day_counts(&pool, untouched.id, noon(anchor, 6))
                .await
                .unwrap()
                .is_empty(),
            "a tag with no statuses has no buckets (the endpoint renders the \
             zeros)"
        );

        // A use older than the seven-day window is out of the read entirely.
        let ancient = post(&pool, "frank", "#rust").await;
        pin_day(&pool, ancient.id, anchor, 30).await;
        assert_eq!(
            week_of(&pool, "rust", anchor).await.len(),
            2,
            "the thirty-day-old use stays out of the week"
        );
        let wide = day_counts(
            &pool,
            get_by_name(&pool, "rust")
                .await
                .unwrap()
                .expect("rust exists")
                .id,
            noon(anchor, 45),
        )
        .await
        .expect("the wide window reads");
        assert_eq!(
            wide.first().expect("buckets came back").0,
            today - Duration::days(30),
            "the window is the caller's: ask wider, get the older day too"
        );
        assert_eq!(
            wide.len(),
            3,
            "the thirty-day day plus the two inside the week"
        );
    }
    /// `public_posts` reads the tag's feed page: the live, `public`, root
    /// posts carrying the tag, newest id first, keyset-paged by `max_id`
    /// and cut by `limit` — the other visibilities, replies, reblogs,
    /// tombstones and untagged posts stay out, and a tag no status has
    /// used answers empty rather than failing.
    #[tokio::test]
    async fn public_posts_pages_the_tagged_public_roots() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = insert_user(&pool, &NewUser::local("alice", &local_ap_id(BASE, "alice")))
            .await
            .expect("fixture user inserts");
        let one = status::insert(&pool, BASE, &NewStatus::local(&author, "one #rust"))
            .await
            .expect("fixture status inserts");
        let _unlisted = {
            let mut new = NewStatus::local(&author, "unlisted #rust");
            new.visibility = Visibility::Unlisted;
            status::insert(&pool, BASE, &new)
                .await
                .expect("fixture status inserts")
        };
        let two = status::insert(&pool, BASE, &NewStatus::local(&author, "two #rust"))
            .await
            .expect("fixture status inserts");
        let _reply = {
            let mut new = NewStatus::local(&author, "in answer #rust");
            new.in_reply_to_id = Some(one.id);
            status::insert(&pool, BASE, &new)
                .await
                .expect("fixture status inserts")
        };
        let three = status::insert(&pool, BASE, &NewStatus::local(&author, "three #rust"))
            .await
            .expect("fixture status inserts");
        let _untagged = status::insert(&pool, BASE, &NewStatus::local(&author, "plain talk"))
            .await
            .expect("fixture status inserts");
        assert!(
            status::soft_delete(&pool, two.id).await.unwrap().is_some(),
            "one tagged public root is deleted"
        );

        let tag = get_by_name(&pool, "rust")
            .await
            .unwrap()
            .expect("the tag row exists");
        let page_ids = |statuses: Vec<Status>| {
            statuses
                .into_iter()
                .map(|found| found.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            page_ids(public_posts(&pool, tag.id, None, 20).await.unwrap()),
            vec![three.id, one.id],
            "newest first, the tombstone gone, the unlisted post and the \
             reply out, and the untagged post never in"
        );
        assert_eq!(
            page_ids(
                public_posts(&pool, tag.id, Some(three.id), 20)
                    .await
                    .unwrap()
            ),
            vec![one.id],
            "the cursor pages strictly older posts"
        );
        assert_eq!(
            page_ids(public_posts(&pool, tag.id, None, 1).await.unwrap()),
            vec![three.id],
            "the limit cuts the page"
        );
    }

    /// A tag with no statuses linked answers the empty page rather than
    /// failing: what makes the feed route's "unknown tag is a valid empty
    /// feed" the ordinary answer.
    #[tokio::test]
    async fn public_posts_of_an_unused_tag_is_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let tag = find_or_create(&pool, "neverused")
            .await
            .unwrap()
            .expect("the tag row is created");
        assert!(
            public_posts(&pool, tag.id, None, 20)
                .await
                .unwrap()
                .is_empty(),
            "a tag no status carries has nothing to show"
        );
    }
}
