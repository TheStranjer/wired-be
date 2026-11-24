//! The `bookmark_folders` table: one account's named folders for filing
//! their bookmarks — a private fact, relationally.
//!
//! Reference: Pleroma's `Pleroma.BookmarkFolder`
//! (`lib/pleroma/bookmark_folder.ex`) over the same-named table; here the
//! row is this table (`docs/ARCHITECTURE.md` decision 3), keyed by the
//! `UUIDv7` the API names (decision 4). Nothing federates: a folder is the
//! account's own shelf, and `name`/`emoji` are free text the owner chose.
//!
//! Folders are strictly per-user: [`get_owned`], [`update`] and [`delete`]
//! all carry the owner in their `WHERE` clause, so a foreign id is
//! indistinguishable from a missing one (both answer `None`), which is
//! exactly the 404 the API owes (see [`crate::api::pleroma_bookmark_folders`]).
//! `(user_id, name)` is unique — the constraint behind the API's duplicate
//! refusal, which arrives here unchanged as the database's own violation
//! (SQLSTATE `23505`) for the caller to recognise.
//!
//! [`update`] is the partial write the API's `PATCH` performs: a `None`
//! (absent) parameter keeps the stored column, so only what the request
//! names changes — the same shape [`crate::models::list::update`] takes.
//!
//! Deleting a folder does not delete the bookmarks filed in it: the
//! `bookmarks.folder_id` foreign key's `ON DELETE SET NULL` frees them.
//! Deleting a user takes their folders through the owner's cascade.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`create`]: the id this module mints (decision 4), the owner and what
/// the caller named, with `inserted_at` and `updated_at` left to the column
/// defaults. `RETURNING *` brings the row back with those stamps.
const INSERT_SQL: &str = "INSERT INTO bookmark_folders (id, user_id, name, emoji)
     VALUES ($1, $2, $3, $4)
   RETURNING *";

/// [`get_owned`]: one folder, only of its owner — a foreign id reads as
/// absent.
const GET_OWNED_SQL: &str = "SELECT * FROM bookmark_folders WHERE user_id = $1 AND id = $2";

/// [`list`]: the owner's folders, oldest first on the id whose text sorts
/// in creation order (decision 4) — the creation order the API lists them
/// in.
const LIST_SQL: &str = "SELECT * FROM bookmark_folders WHERE user_id = $1 ORDER BY id";

/// [`update`]: only the columns the request names — an absent (`None`)
/// parameter is bound `NULL` and `COALESCE`d away, keeping the stored
/// value — and the `updated_at` bump Pleroma's `Repo.update` stamps too.
/// The owner is part of the `WHERE` clause, so [`update`] reports a foreign
/// or missing row as `None` without a separate existence read.
const UPDATE_SQL: &str = "UPDATE bookmark_folders
      SET name = COALESCE($3, name),
          emoji = COALESCE($4, emoji),
          updated_at = now()
    WHERE user_id = $1 AND id = $2
 RETURNING *";

/// [`delete`]: one owned folder, `RETURNING *` so the API can answer the
/// entity it just removed. The `bookmarks.folder_id` foreign key's
/// `ON DELETE SET NULL` frees the filed bookmarks with it; a foreign or
/// missing id deletes nothing and returns no row.
const DELETE_SQL: &str = "DELETE FROM bookmark_folders
      WHERE user_id = $1 AND id = $2
   RETURNING *";

/// One row of `bookmark_folders`: a named shelf one account files their
/// bookmarks on.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct BookmarkFolder {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); also the
    /// creation order [`list`] reads in.
    pub id: Uuid,
    /// The owning account; every read and write of a folder is scoped to
    /// it.
    pub user_id: Uuid,
    /// The user-defined name, verbatim as its owner wrote it.
    pub name: String,
    /// The folder's emoji as stored: one unicode emoji or a `:shortcode:`
    /// naming a local custom emoji, `None` for a folder wearing none.
    pub emoji: Option<String>,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every [`update`].
    pub updated_at: DateTime<Utc>,
}

/// What to change: the present-only parameters of the API's `PATCH`. A
/// `None` field keeps the stored column; [`update`] applies exactly the
/// named ones.
#[derive(Clone, Debug, Default)]
pub struct FolderChanges {
    /// The new name; `None` keeps the stored one.
    pub name: Option<String>,
    /// The new emoji; `None` keeps the stored one.
    pub emoji: Option<String>,
}

/// Creates a folder named `name` (with the optional `emoji`) owned by
/// `user_id` and returns the stored row.
///
/// The primary key is minted here (decision 4). A name the owner already
/// uses on another folder violates the table's unique constraint and
/// arrives as [`AppError::Database`](crate::error::AppError) carrying
/// SQLSTATE `23505` — the endpoint's duplicate 422 reads that code off it.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    emoji: Option<&str>,
) -> AppResult<BookmarkFolder> {
    let row = sqlx::query_as::<_, BookmarkFolder>(INSERT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(name)
        .bind(emoji)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Fetches the folder `id` if `user_id` owns it; `None` for a missing or
/// foreign id, which the API renders as one and the same 404.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
) -> AppResult<Option<BookmarkFolder>> {
    let row = sqlx::query_as::<_, BookmarkFolder>(GET_OWNED_SQL)
        .bind(user_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The folders of `user_id`, oldest first (creation order, decision 4) —
/// what `GET /api/v1/pleroma/bookmark_folders` answers.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<BookmarkFolder>> {
    let rows = sqlx::query_as::<_, BookmarkFolder>(LIST_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Applies `changes` to the folder `id` of `user_id` — the present-only
/// write the API's `PATCH` performs, every absent field keeping its stored
/// value.
///
/// Returns the rewritten row, `None` when no owned folder carries that id.
/// Even a wholly empty `changes` bumps `updated_at`, the same row Pleroma's
/// no-op `Repo.update` answers. Renaming onto a name the owner already
/// uses violates the table's unique constraint and arrives as
/// [`AppError::Database`](crate::error::AppError) carrying SQLSTATE `23505`.
///
/// # Errors
///
/// As described; any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
    changes: &FolderChanges,
) -> AppResult<Option<BookmarkFolder>> {
    let row = sqlx::query_as::<_, BookmarkFolder>(UPDATE_SQL)
        .bind(user_id)
        .bind(id)
        .bind(&changes.name)
        .bind(&changes.emoji)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Deletes the folder `id` of `user_id` and returns the row that went;
/// `None` when a foreign or missing id deletes nothing.
///
/// The bookmarks filed in it are kept: the `bookmarks.folder_id` foreign
/// key's `ON DELETE SET NULL` un-files them while the rows stand (Pleroma
/// leans on the same database rule).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<BookmarkFolder>> {
    let row = sqlx::query_as::<_, BookmarkFolder>(DELETE_SQL)
        .bind(user_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            bookmark,
            status::{NewStatus, insert as insert_status},
            user::User,
        },
        test_fixtures::local_user as user,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// A folder named `name` owned by `owner`.
    async fn seed(pool: &PgPool, owner: &User, name: &str) -> BookmarkFolder {
        create(pool, owner.id, name, None)
            .await
            .expect("the fixture folder creates")
    }

    /// The `bookmark_folders` rows of `user_id` as `(id, name)`, creation
    /// order — what a list assertion compares against.
    async fn stored(pool: &PgPool, user_id: Uuid) -> Vec<(Uuid, String)> {
        sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, name FROM bookmark_folders WHERE user_id = $1 ORDER BY id",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await
        .expect("the table reads")
    }

    /// Whether `err` is the database's own unique violation (`23505`).
    fn is_unique_violation(err: &AppError) -> bool {
        matches!(err, AppError::Database(sqlx::Error::Database(db))
            if db.code().as_deref() == Some("23505"))
    }

    #[tokio::test]
    async fn create_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        let stored = create(&pool, alice.id, "Read later", Some("📚"))
            .await
            .expect("a folder creates");

        assert!(stored.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(stored.user_id, alice.id, "the owner is the argument");
        assert_eq!(stored.name, "Read later", "the name is kept verbatim");
        assert_eq!(stored.emoji.as_deref(), Some("📚"), "the emoji round trips");
        assert_eq!(
            stored.inserted_at, stored.updated_at,
            "a fresh row is stamped once"
        );

        let plain = seed(&pool, &alice, "No emoji").await;
        assert_eq!(plain.emoji, None, "and a folder may wear none");
    }

    #[tokio::test]
    async fn a_second_folder_with_the_same_name_violates_the_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let _dup = seed(&pool, &alice, "dup").await;

        let err = create(&pool, alice.id, "dup", None)
            .await
            .expect_err("the owner's second `dup` cannot store");
        assert!(is_unique_violation(&err), "the key refuses: {err}");
        assert_eq!(
            stored(&pool, alice.id).await.len(),
            1,
            "and nothing was written"
        );

        // The uniqueness is per owner: bob may keep his own `dup`.
        let bob = user(&pool, "bob").await;
        let his = create(&pool, bob.id, "dup", None)
            .await
            .expect("a different owner may reuse the name");
        assert_eq!(his.name, "dup", "and the row stores");
    }

    #[tokio::test]
    async fn list_is_oldest_first_and_the_owners_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = seed(&pool, &alice, "first").await;
        let second = seed(&pool, &alice, "second").await;
        let theirs = seed(&pool, &bob, "theirs").await;

        let listed = list(&pool, alice.id).await.expect("the list reads");
        assert_eq!(
            listed,
            vec![first, second],
            "alice's two folders, oldest first; bob's stays his"
        );
        assert_eq!(theirs.user_id, bob.id, "and bob's was there to be missed");

        assert!(
            list(&pool, user(&pool, "lonely").await.id)
                .await
                .expect("an account with no folders reads")
                .is_empty(),
            "nobody's shelf is the empty list"
        );
    }

    #[tokio::test]
    async fn get_owned_reads_the_owners_row_and_nobody_elses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "mine").await;

        let fetched = get_owned(&pool, alice.id, hers.id)
            .await
            .expect("the owner's read succeeds");
        assert_eq!(fetched, Some(hers.clone()), "the owner gets the whole row");

        let foreign = get_owned(&pool, bob.id, hers.id)
            .await
            .expect("a foreign read succeeds too");
        assert!(foreign.is_none(), "and reports the row as absent");

        let missing = get_owned(&pool, alice.id, Uuid::now_v7())
            .await
            .expect("reading nothing is not a failure");
        assert!(missing.is_none(), "an unknown id reads the same way");
    }

    #[tokio::test]
    async fn update_changes_only_the_present_columns() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = create(&pool, alice.id, "before", Some("📚"))
            .await
            .expect("the fixture creates");

        let renamed = update(
            &pool,
            alice.id,
            stored.id,
            &FolderChanges {
                name: Some("after".to_owned()),
                emoji: None,
            },
        )
        .await
        .expect("a folder updates")
        .expect("the fixture folder exists");

        assert_eq!(renamed.id, stored.id, "the row is rewritten in place");
        assert_eq!(renamed.user_id, alice.id, "and stays owned");
        assert_eq!(renamed.name, "after", "the named name is replaced");
        assert_eq!(
            renamed.emoji.as_deref(),
            Some("📚"),
            "the absent emoji keeps its value"
        );
        assert_eq!(renamed.inserted_at, stored.inserted_at, "stays original");
        assert!(
            renamed.updated_at >= stored.updated_at,
            "the update stamp moves forward"
        );

        // The emoji arm rewrites too, and an absent name keeps its value.
        let other = seed(&pool, &alice, "other").await;
        let emoji_named = update(
            &pool,
            alice.id,
            other.id,
            &FolderChanges {
                name: None,
                emoji: Some(":cat:".to_owned()),
            },
        )
        .await
        .expect("the second update runs")
        .expect("the folder is there");
        assert_eq!(emoji_named.name, "other", "the absent name kept");
        assert_eq!(
            emoji_named.emoji.as_deref(),
            Some(":cat:"),
            "and the named emoji replaced"
        );
    }

    #[tokio::test]
    async fn update_onto_a_taken_name_violates_the_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let _taken = seed(&pool, &alice, "taken").await;
        let other = seed(&pool, &alice, "other").await;

        let err = update(
            &pool,
            alice.id,
            other.id,
            &FolderChanges {
                name: Some("taken".to_owned()),
                emoji: None,
            },
        )
        .await
        .expect_err("the owner's taken name cannot be renamed onto");
        assert!(is_unique_violation(&err), "the key refuses: {err}");
        assert_eq!(
            get_owned(&pool, alice.id, other.id)
                .await
                .expect("the row reads")
                .expect("the folder was there")
                .name,
            "other",
            "and the refused update changed nothing"
        );
    }

    #[tokio::test]
    async fn update_of_a_foreign_or_missing_id_does_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "mine").await;

        let changes = FolderChanges {
            name: Some("stolen".to_owned()),
            emoji: None,
        };
        assert!(
            update(&pool, bob.id, hers.id, &changes)
                .await
                .expect("a foreign update is not a failure")
                .is_none(),
            "and rewrites nothing"
        );
        assert!(
            update(&pool, alice.id, Uuid::now_v7(), &changes)
                .await
                .expect("updating nothing is not a failure")
                .is_none(),
            "no row, no rewrite"
        );
        assert_eq!(
            get_owned(&pool, alice.id, hers.id)
                .await
                .expect("the row reads")
                .expect("the folder survived")
                .name,
            "mine",
            "and her folder kept its name"
        );
    }

    #[tokio::test]
    async fn delete_answers_the_row_once_and_frees_its_bookmarks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status = insert_status(&pool, BASE, &NewStatus::local(&bob, "hello"))
            .await
            .expect("the fixture status inserts");
        let folder = seed(&pool, &alice, "doomed").await;
        let filed = bookmark::add(&pool, alice.id, status.id, Some(folder.id))
            .await
            .expect("the fixture bookmark files");
        assert_eq!(filed.folder_id, Some(folder.id), "filed before freed");

        let gone = delete(&pool, alice.id, folder.id)
            .await
            .expect("the folder deletes")
            .expect("the first delete finds it");
        assert_eq!(gone, folder, "the answer is the row that went");
        assert!(
            delete(&pool, alice.id, folder.id)
                .await
                .expect("deleting nothing is not a failure")
                .is_none(),
            "the second finds nothing"
        );

        // The bookmark stands, unfiled: `ON DELETE SET NULL` frees it.
        let row: (Option<Uuid>,) = sqlx::query_as("SELECT folder_id FROM bookmarks WHERE id = $1")
            .bind(filed.id)
            .fetch_one(&pool)
            .await
            .expect("the bookmark row reads");
        assert_eq!(row.0, None, "the kept bookmark lost its folder");
        assert_eq!(
            bookmark::bookmarked_folders(&pool, alice.id, &[status.id])
                .await
                .expect("the flags read"),
            std::iter::once((status.id, None)).collect(),
            "and it is still a bookmark, just unfiled"
        );
    }

    #[tokio::test]
    async fn delete_of_a_foreign_or_missing_id_does_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "mine").await;

        assert!(
            delete(&pool, bob.id, hers.id)
                .await
                .expect("a foreign delete is not a failure")
                .is_none(),
            "bob deletes nothing of alice's"
        );
        assert!(
            delete(&pool, alice.id, Uuid::now_v7())
                .await
                .expect("deleting nothing is not a failure")
                .is_none(),
            "and an unknown id deletes nothing"
        );
        assert!(
            get_owned(&pool, alice.id, hers.id)
                .await
                .expect("the row reads")
                .is_some(),
            "alice's folder survived every failed attempt on it"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_cascades_their_folders() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let _hers = seed(&pool, &alice, "hers").await;
        let _his = seed(&pool, &bob, "his").await;

        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the owner was there to delete");
        assert!(
            stored(&pool, alice.id).await.is_empty(),
            "and the cascade took her folders"
        );
        assert_eq!(
            list(&pool, bob.id).await.expect("bob's list reads").len(),
            1,
            "while bob's folder stands"
        );
    }
}
