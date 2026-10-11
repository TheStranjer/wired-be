//! The `lists` table (and its `list_members` join table): per-user named
//! collections of accounts with Mastodon v1 semantics.
//!
//! Reference: Pleroma's `Pleroma.List` (`lib/pleroma/list.ex`). Pleroma
//! stores membership as a `following` array of follower addresses on the
//! list row and derives an `ap_id` per list; neither is needed here: the
//! row's `UUIDv7` id (decision 4) is what the API names, and membership is
//! the `list_members` table, keyed by the member's `users` row (decision 7)
//! — which is also what makes [`lists_containing`] a plain join. The
//! `replies_policy` column has no Pleroma 2.10 counterpart; it is
//! Mastodon's, and the API validates its value while the migration's check
//! constraint keeps the column honest.
//!
//! Lists are strictly per-user: [`get_owned`], [`update`] and [`delete`]
//! all carry the owner in their `WHERE` clause, so a foreign id is
//! indistinguishable from a missing one (both answer `None`/`false`), which
//! is exactly the 404 the API owes. Deleting a user takes their lists —
//! and, through the members' cascade, their memberships everywhere — with
//! it; deleting a list takes its own memberships with it.
//!
//! [`update`] is the partial write the API's `PUT` performs: a `NULL`
//! (absent) parameter keeps the stored column, so only what the request
//! names changes.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

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

/// [`lists_containing`]: the owner's lists that hold `member_id`, oldest
/// first — Pleroma's `get_lists_account_belongs`, expressed as the join
/// the membership table is made for.
const LISTS_CONTAINING_SQL: &str = "SELECT lists.* FROM lists
     JOIN list_members ON list_members.list_id = lists.id
     WHERE lists.user_id = $1 AND list_members.user_id = $2
     ORDER BY lists.id";

/// One row of `lists`: a named collection of accounts one user keeps.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct List {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); also the
    /// creation order lists are read in.
    pub id: Uuid,
    /// The owning account; every read and write of a list is scoped to it.
    pub user_id: Uuid,
    /// The user-defined title, verbatim as its owner wrote it.
    pub title: String,
    /// Whose replies the list shows: one of `none`, `list` or `followed`
    /// (the API validates; the column stores what passed).
    pub replies_policy: String,
    /// Whether members are removed from the owner's Home feed
    /// (Mastodon's and Pleroma's `exclusive`).
    pub exclusive: bool,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every [`update`].
    pub updated_at: DateTime<Utc>,
}

/// What to create: every `lists` column except the `id` [`create`] mints,
/// the `user_id` the handler takes from its caller and the timestamps the
/// database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets only
/// what it decides on; the usual minimum is the title.
#[derive(Clone, Debug)]
pub struct NewList {
    /// The title; empty in the default.
    pub title: String,
    /// Whose replies the list shows; `"list"` by default, like the column.
    pub replies_policy: String,
    /// Whether members leave the Home feed; `false` by default, like the
    /// column.
    pub exclusive: bool,
}

impl Default for NewList {
    /// The column defaults: empty title, the `list` replies policy and
    /// exclusiveness off.
    fn default() -> Self {
        Self {
            title: String::new(),
            replies_policy: "list".to_owned(),
            exclusive: false,
        }
    }
}

/// What to change: the present-only parameters of the API's `PUT`. A `None`
/// field keeps the stored column; [`update`] applies exactly the named
/// ones.
#[derive(Clone, Debug, Default)]
pub struct ListChanges {
    /// The new title; `None` keeps the stored one.
    pub title: Option<String>,
    /// The new replies policy; `None` keeps the stored one.
    pub replies_policy: Option<String>,
    /// The new exclusiveness; `None` keeps the stored one.
    pub exclusive: Option<bool>,
}

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

/// The lists `owner_id` owns that hold `member_id`, oldest first — what
/// `GET /api/v1/accounts/:id/lists` answers for its viewer.
///
/// A member of nobody's list, or of only lists of other owners, yields the
/// empty vector; whether `member_id` names a live user is the API's
/// question, not this query's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn lists_containing(
    pool: &PgPool,
    owner_id: Uuid,
    member_id: Uuid,
) -> AppResult<Vec<List>> {
    let rows = sqlx::query_as::<_, List>(LISTS_CONTAINING_SQL)
        .bind(owner_id)
        .bind(member_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, User, insert, local_ap_id},
    };

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A list titled `title` with the defaults otherwise.
    fn titled(title: &str) -> NewList {
        NewList {
            title: title.to_owned(),
            ..NewList::default()
        }
    }

    /// Inserts a list titled `title` for `user`, dropping the row.
    async fn seed(pool: &PgPool, owner: &User, title: &str) -> List {
        create(pool, owner.id, &titled(title))
            .await
            .expect("the fixture list creates")
    }

    /// Adds `member` to `list` straight through the join table (the
    /// membership endpoints are a later ticket; the model only reads this
    /// table).
    async fn add_member(pool: &PgPool, list: &List, member: &User) {
        let inserted = sqlx::query("INSERT INTO list_members (list_id, user_id) VALUES ($1, $2)")
            .bind(list.id)
            .bind(member.id)
            .execute(pool)
            .await
            .expect("the fixture membership inserts");
        assert_eq!(
            inserted.rows_affected(),
            1,
            "exactly one membership went in"
        );
    }

    /// The titles of `rows`, in row order — what a list test compares
    /// against.
    fn titles(rows: &[List]) -> Vec<&str> {
        rows.iter().map(|row| row.title.as_str()).collect()
    }

    /// How many `lists` rows the database holds.
    async fn count_lists(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM lists")
            .fetch_one(pool)
            .await
            .expect("counting lists succeeds")
    }

    /// How many `list_members` rows the database holds.
    async fn count_members(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM list_members")
            .fetch_one(pool)
            .await
            .expect("counting memberships succeeds")
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_round_trips_every_column(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_defaults_match_the_column_defaults(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_owned_reads_the_owners_row_and_nobody_elses(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn for_user_is_oldest_first_and_the_owners_own(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_changes_only_the_present_columns(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_replaces_every_named_column(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_with_nothing_named_keeps_the_row(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_and_delete_of_a_foreign_or_missing_id_do_nothing(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn delete_takes_a_list_once(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn lists_containing_answers_the_owners_membership_in_order(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = seed(&pool, &alice, "first").await;
        let _skipped: List = seed(&pool, &alice, "without").await;
        let last = seed(&pool, &alice, "last").await;
        add_member(&pool, &first, &bob).await;
        add_member(&pool, &last, &bob).await;

        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &carol, "carol's own").await;
        add_member(&pool, &hers, &bob).await;

        let found = lists_containing(&pool, alice.id, bob.id)
            .await
            .expect("the lookup reads");
        assert_eq!(
            titles(&found),
            ["first", "last"],
            "alice's two lists holding bob, oldest first; carol's list of \
             bob is not alice's, and alice's list without bob is no match"
        );

        let nolists = lists_containing(&pool, alice.id, carol.id)
            .await
            .expect("reading nobody's member is not a failure");
        assert!(
            nolists.is_empty(),
            "a member of no list yields the empty vector"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn membership_is_idempotent_only_through_the_key(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;

        let refused = sqlx::query("INSERT INTO list_members (list_id, user_id) VALUES ($1, $2)")
            .bind(hers.id)
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect_err("the primary key refuses a second row");
        assert!(
            matches!(refused, sqlx::Error::Database(_)),
            "and the refusal is the database's own: {refused}"
        );
        assert_eq!(
            lists_containing(&pool, alice.id, bob.id)
                .await
                .expect("the lookup reads")
                .len(),
            1,
            "and the lookup answers the list once"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_a_list_takes_its_memberships(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let doomed = seed(&pool, &alice, "doomed").await;
        add_member(&pool, &doomed, &bob).await;
        assert_eq!(count_members(&pool).await, 1, "one membership");

        assert!(
            delete(&pool, alice.id, doomed.id)
                .await
                .expect("the list deletes"),
            "and it goes"
        );
        assert_eq!(
            count_members(&pool).await,
            0,
            "taking its memberships with it"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_a_user_takes_their_lists_and_memberships(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");

        assert_eq!(count_lists(&pool).await, 0, "and her lists with it");
        assert_eq!(
            count_members(&pool).await,
            0,
            "and every membership in them"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_a_member_takes_their_rows(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture member deletes");
        assert_eq!(deleted.rows_affected(), 1, "the account went");
        assert_eq!(
            count_members(&pool).await,
            0,
            "and its memberships everywhere"
        );
        assert_eq!(count_lists(&pool).await, 1, "while alice's list stands");
    }
}
