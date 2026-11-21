//! The `lists` rows themselves: [`create`] mints one, [`get_owned`] and
//! [`for_user`] read an owner's rows, [`update`] is the partial write the API's
//! `PUT` performs, and [`delete`] and [`delete_all_for_user`] take them away.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

use super::{List, ListChanges, NewList};

/// [`create`]: the id this module mints (decision 4), the owner and what
/// the caller decided, with `inserted_at` and `updated_at` left to the
/// column defaults. `RETURNING *` brings the row back with those stamps.
const INSERT_SQL: &str = "INSERT INTO lists (
     id, user_id, title, replies_policy, exclusive
) VALUES ($1, $2, $3, $4, $5) RETURNING *";

/// [`get_owned`]: one list, only of its owner — a foreign id reads as
/// absent.
const GET_OWNED_SQL: &str = "SELECT * FROM lists WHERE user_id = $1 AND id = $2";

/// [`for_user`]: the owner's lists, oldest first on the id whose text sorts
/// in creation order (decision 4) — the creation order the API lists them
/// in.
const FOR_USER_SQL: &str = "SELECT * FROM lists WHERE user_id = $1 ORDER BY id";

/// [`update`]: only the columns the request names — an absent (`None`)
/// parameter is bound `NULL` and `COALESCE`d away, keeping the stored
/// value — and the `updated_at` bump Pleroma's `Repo.update` stamps too.
/// The owner is part of the `WHERE` clause, so [`update`] reports a foreign
/// or missing row as `None` without a separate existence read.
const UPDATE_SQL: &str = "UPDATE lists
     SET title = COALESCE($3, title),
         replies_policy = COALESCE($4, replies_policy),
         exclusive = COALESCE($5, exclusive),
         updated_at = now()
     WHERE user_id = $1 AND id = $2
  RETURNING *";

/// [`delete`]: one owned list; a foreign or missing id deletes nothing,
/// and the members' cascade takes the list's `list_members` rows with it.
const DELETE_SQL: &str = "DELETE FROM lists WHERE user_id = $1 AND id = $2";

/// Inserts `list` owned by `user_id` and returns the stored row.
///
/// The primary key is minted here (decision 4), so the row's place in the
/// creation-order list is its creation time.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, user_id: Uuid, list: &NewList) -> AppResult<List> {
    let row = sqlx::query_as::<_, List>(INSERT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(&list.title)
        .bind(&list.replies_policy)
        .bind(list.exclusive)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Fetches the list `id` if `user_id` owns it; `None` for a missing or
/// foreign id, which the API renders as one and the same 404.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<List>> {
    let row = sqlx::query_as::<_, List>(GET_OWNED_SQL)
        .bind(user_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The lists of `user_id`, oldest first (creation order, decision 4) —
/// what `GET /api/v1/lists` answers.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_user(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<List>> {
    let rows = sqlx::query_as::<_, List>(FOR_USER_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Applies `changes` to the list `id` of `user_id` — the present-only
/// write the API's `PUT` performs, every absent field keeping its stored
/// value.
///
/// Returns the rewritten row, `None` when no owned list carries that id.
/// Even a wholly empty `changes` bumps `updated_at`, the same row
/// Pleroma's no-op `Repo.update` answers.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
    changes: &ListChanges,
) -> AppResult<Option<List>> {
    let row = sqlx::query_as::<_, List>(UPDATE_SQL)
        .bind(user_id)
        .bind(id)
        .bind(&changes.title)
        .bind(&changes.replies_policy)
        .bind(changes.exclusive)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Deletes the list `id` of `user_id`, reporting whether one went; a
/// foreign or missing id deletes nothing and reports `false`. The list's
/// memberships go with it through the members' cascade.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_SQL)
        .bind(user_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// The `DELETE` behind [`delete_all_for_user`]: the account's own lists.
const DELETE_ALL_FOR_USER_SQL: &str = "DELETE FROM lists WHERE user_id = $1";

/// The `DELETE` behind [`delete_all_for_user`]: the account's memberships
/// of the lists it does not own.
const REMOVE_MEMBERSHIPS_SQL: &str = "DELETE FROM list_members WHERE user_id = $1";

/// Removes every list the account owns and every membership of it: the
/// list step of the [`users.delete`](crate::jobs::deletion::KIND_DELETE)
/// job.
///
/// The owned lists' own member rows go with them through the members'
/// cascade, before the second statement runs, so what it clears is exactly
/// the memberships in other accounts' lists — both foreign keys pointing at
/// the departing row, which the schema would cascade on a hard delete.
/// Returns the number of rows taken from the two tables together.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let owned = sqlx::query(DELETE_ALL_FOR_USER_SQL)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    let memberships = sqlx::query(REMOVE_MEMBERSHIPS_SQL)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(owned + memberships)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::list::test_support::*;

    #[tokio::test]
    async fn create_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = create(
            &pool,
            alice.id,
            &NewList {
                title: "Close friends".to_owned(),
                replies_policy: "followed".to_owned(),
                exclusive: true,
            },
        )
        .await
        .expect("a list creates");

        assert!(stored.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(stored.user_id, alice.id, "the owner is the argument");
        assert_eq!(stored.title, "Close friends", "the title is kept verbatim");
        assert_eq!(
            stored.replies_policy, "followed",
            "the replies policy round trips"
        );
        assert!(stored.exclusive, "the exclusiveness round trips");
        assert_eq!(
            stored.inserted_at, stored.updated_at,
            "a fresh row is stamped once"
        );
    }

    #[tokio::test]
    async fn create_defaults_match_the_column_defaults() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = create(&pool, alice.id, &titled("x"))
            .await
            .expect("a bare list creates");
        assert_eq!(
            stored.replies_policy, "list",
            "the replies policy is `list` by default, like the column"
        );
        assert!(
            !stored.exclusive,
            "exclusiveness is off by default, like the column"
        );

        let empty = NewList::default();
        assert!(empty.title.is_empty(), "the default title is empty");
        assert_eq!(
            empty.replies_policy, "list",
            "and the default policy mirrors the column"
        );
    }

    #[tokio::test]
    async fn get_owned_reads_the_owners_row_and_nobody_elses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = create(&pool, alice.id, &titled("mine"))
            .await
            .expect("the fixture creates");

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
    async fn for_user_is_oldest_first_and_the_owners_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first: List = seed(&pool, &alice, "first").await;
        let second: List = seed(&pool, &alice, "second").await;
        let theirs: List = seed(&pool, &bob, "theirs").await;
        assert_eq!(first.title, "first", "the fixtures are in place");
        assert_eq!(second.title, "second", "for three users");
        assert_eq!(theirs.title, "theirs", "and two lists each");

        let listed = for_user(&pool, alice.id).await.expect("the list reads");
        assert_eq!(
            titles(&listed),
            ["first", "second"],
            "alice's two lists, oldest first; bob's stays his"
        );
    }

    #[tokio::test]
    async fn update_changes_only_the_present_columns() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = create(
            &pool,
            alice.id,
            &NewList {
                title: "before".to_owned(),
                replies_policy: "none".to_owned(),
                exclusive: true,
            },
        )
        .await
        .expect("the fixture creates");

        let rewritten = update(
            &pool,
            alice.id,
            stored.id,
            &ListChanges {
                title: Some("after".to_owned()),
                replies_policy: None,
                exclusive: None,
            },
        )
        .await
        .expect("a list updates")
        .expect("the fixture list exists");

        assert_eq!(rewritten.id, stored.id, "the row is rewritten in place");
        assert_eq!(rewritten.user_id, alice.id, "and stays owned");
        assert_eq!(rewritten.title, "after", "the named title is replaced");
        assert_eq!(
            rewritten.replies_policy, "none",
            "the absent policy keeps its value"
        );
        assert!(rewritten.exclusive, "and so does the absent flag");
        assert_eq!(
            rewritten.inserted_at, stored.inserted_at,
            "the creation stamp stays"
        );
        assert!(
            rewritten.updated_at >= stored.updated_at,
            "the update stamp moves forward"
        );
    }

    #[tokio::test]
    async fn update_replaces_every_named_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = seed(&pool, &alice, "before").await;

        let rewritten = update(
            &pool,
            alice.id,
            stored.id,
            &ListChanges {
                title: Some("after".to_owned()),
                replies_policy: Some("none".to_owned()),
                exclusive: Some(true),
            },
        )
        .await
        .expect("the update succeeds")
        .expect("the fixture exists");

        assert_eq!(rewritten.title, "after", "the named title");
        assert_eq!(rewritten.replies_policy, "none", "the named policy");
        assert!(rewritten.exclusive, "and the named flag all changed");
    }

    #[tokio::test]
    async fn update_with_nothing_named_keeps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = seed(&pool, &alice, "kept").await;

        let rewritten = update(&pool, alice.id, stored.id, &ListChanges::default())
            .await
            .expect("the update succeeds")
            .expect("the fixture exists");

        assert_eq!(rewritten.title, "kept", "nothing was named,");
        assert_eq!(rewritten.replies_policy, "list", "nothing changed:");
        assert!(!rewritten.exclusive, "the row keeps every column");
    }

    #[tokio::test]
    async fn update_and_delete_of_a_foreign_or_missing_id_do_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = create(&pool, alice.id, &titled("mine"))
            .await
            .expect("the fixture creates");

        let stolen = ListChanges {
            title: Some("stolen".to_owned()),
            replies_policy: None,
            exclusive: None,
        };
        let updated = update(&pool, bob.id, hers.id, &stolen)
            .await
            .expect("a foreign update is not a failure");
        assert!(updated.is_none(), "and rewrites nothing");

        let missing = Uuid::now_v7();
        assert!(
            update(&pool, alice.id, missing, &stolen)
                .await
                .expect("updating nothing is not a failure")
                .is_none(),
            "no row, no rewrite"
        );

        assert!(
            !(delete(&pool, bob.id, hers.id)
                .await
                .expect("a foreign delete is not a failure")),
            "bob deletes nothing of alice's"
        );
        assert!(
            !(delete(&pool, alice.id, missing)
                .await
                .expect("deleting nothing is not a failure")),
            "and an unknown id deletes nothing"
        );
        assert!(
            get_owned(&pool, alice.id, hers.id)
                .await
                .expect("the row reads")
                .is_some(),
            "alice's list survived every failed attempt on it"
        );
    }

    #[tokio::test]
    async fn delete_takes_a_list_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let doomed = seed(&pool, &alice, "doomed").await;

        assert!(
            delete(&pool, alice.id, doomed.id)
                .await
                .expect("the list deletes"),
            "the first delete finds it"
        );
        assert!(
            !(delete(&pool, alice.id, doomed.id)
                .await
                .expect("deleting nothing is not a failure")),
            "the second finds nothing"
        );
        assert!(
            get_owned(&pool, alice.id, doomed.id)
                .await
                .expect("the row reads")
                .is_none(),
            "and it is gone"
        );
    }
}
