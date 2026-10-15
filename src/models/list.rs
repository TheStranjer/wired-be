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
//!
//! A list's members are written by [`add_members`] and [`remove_members`] and
//! read by [`members_page`] and [`member_ids`]. A list's own order is the order
//! its members were added in, which no `UUIDv7` column of this table can state
//! (the member's id orders the accounts, not the memberships), so
//! `list_members.position` — a `bigserial` ordinal the database fills on
//! insert — is what [`members_page`] keys its `max_id`/`min_id`/`since_id`
//! cursors on, the same column Mastodon's list-accounts page keys on.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, QueryBuilder, postgres::Postgres};
use uuid::Uuid;

use crate::{
    error::AppResult, models::user::User, pagination::OrdinalPageParams, util::id::new_id,
};

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

/// [`add_members`]: every requested member in one statement, each row taking
/// the next `position` from the sequence (so the array's order is the order the
/// members were added in). A pair that is already a member conflicts on the
/// primary key and is left exactly where it was — Pleroma's `Enum.uniq` on the
/// list's address array, expressed as the upsert this table's key allows, and
/// what makes adding twice succeed without writing a second row.
const ADD_MEMBERS_SQL: &str = "INSERT INTO list_members (list_id, user_id)
     SELECT $1::uuid, id FROM unnest($2::uuid[]) AS requested(id)
ON CONFLICT (list_id, user_id) DO NOTHING";

/// [`remove_members`]: the memberships of the given accounts in this one list.
/// An id that is not a member matches nothing and changes nothing, which is
/// Pleroma's `List.unfollow` being content with an address the array never
/// held.
const REMOVE_MEMBERS_SQL: &str = "DELETE FROM list_members
     WHERE list_id = $1 AND user_id = ANY($2)";

/// [`members_page`]: one page of a list's members as their `users` rows, each
/// entry led by the `position` of the membership row behind it — the ordinal
/// the page keys on. The text stops where its one placeholder belongs, because
/// that is where [`members_page`] pushes its bind and the builder is what
/// numbers it `$1`; the keyset it then appends runs on `lm.position`.
const MEMBERS_PAGE_SQL: &str = "SELECT lm.position, u.* FROM list_members lm
     JOIN users u ON u.id = lm.user_id
     WHERE lm.list_id = ";

/// [`member_ids`]: every member of one list, in the order they were added —
/// the whole membership, which is what a later federation ticket walks when it
/// delivers a status to a list.
const MEMBER_IDS_SQL: &str = "SELECT user_id FROM list_members
     WHERE list_id = $1 ORDER BY position";

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

/// One entry of a paged member list: the member's [`User`] row plus the
/// `position` of the `list_members` row that put them on the page.
///
/// The membership ordinal — not the member's id — is what [`members_page`]
/// keys on (WBE-104: the order members were added in is the list's own order),
/// so the endpoint's `max_id`/`min_id`/`since_id` cursors and its `Link`
/// header carry [`MemberEntry::position`] values, as
/// [`crate::models::follow::FollowPageEntry`] carries follow row ids.
#[derive(Debug, sqlx::FromRow)]
pub struct MemberEntry {
    /// `list_members.position`: the sequence ordinal the membership was
    /// inserted at, and so the member's place in the list's own order.
    pub position: i64,
    /// The member's account row.
    #[sqlx(flatten)]
    pub user: User,
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

/// Adds `member_ids` to the list `list_id`, returning how many memberships
/// were new.
///
/// Every id takes the next [`list_members.position`][MemberEntry::position]
/// ordinal, in the order given, so the array's order is the order the accounts
/// joined the list in. An id that is already a member is kept as it was — its
/// position included, which is why re-adding a member is idempotent and does
/// not move them to the end of the list. An empty `member_ids` writes nothing.
///
/// The owner's right to the list, and Mastodon's rule that only followed
/// accounts may be added, are the API's question (see
/// [`crate::api::lists`]); this write trusts its caller and only needs the list
/// row to exist, which the `list_id` foreign key enforces.
///
/// # Errors
///
/// Any database failure (connection, encoding, a list that does not exist)
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn add_members(pool: &PgPool, list_id: Uuid, member_ids: &[Uuid]) -> AppResult<u64> {
    let inserted = sqlx::query(ADD_MEMBERS_SQL)
        .bind(list_id)
        .bind(member_ids)
        .execute(pool)
        .await?;
    Ok(inserted.rows_affected())
}

/// Removes `member_ids` from the list `list_id`, returning how many
/// memberships went away.
///
/// An id that is not a member of this list — or that is not a member of any
/// list at all — matches no row and changes nothing (Pleroma's `List.unfollow`
/// on an address the list's array never held), so removing twice removes once
/// and the second call answers `0`. An empty `member_ids` matches nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remove_members(pool: &PgPool, list_id: Uuid, member_ids: &[Uuid]) -> AppResult<u64> {
    let deleted = sqlx::query(REMOVE_MEMBERS_SQL)
        .bind(list_id)
        .bind(member_ids)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// The members of `list_id`, newest membership first and `page`'s `limit` at a
/// time, each entry carrying its [`MemberEntry::position`] next to the [`User`]
/// (see [`MemberEntry`]).
///
/// [`OrdinalPageParams`]'s cursors are position ordinals, and the page is
/// ordered by them: the order the accounts were added in, newest first — the
/// insertion order the membership's own `position` sequence records, not the
/// members' `UUIDv7` ids (decision 4's rule applies to id-keyed listings).
/// `limit=0` answers every member.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn members_page(
    pool: &PgPool,
    list_id: Uuid,
    page: OrdinalPageParams,
) -> AppResult<Vec<MemberEntry>> {
    let mut builder = QueryBuilder::<Postgres>::new(MEMBERS_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(list_id);
    page.apply_keyset(&mut builder, "lm.position");
    let rows = builder
        .build_query_as::<MemberEntry>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows))
}

/// The ids of `list_id`'s members, oldest membership first — the whole
/// membership, unordered by anything but the order it was added in.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn member_ids(pool: &PgPool, list_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(MEMBER_IDS_SQL)
        .bind(list_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::user::{NewUser, User, insert, local_ap_id},
        pagination::MAX_LIMIT,
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

    /// Adds `member` to `list` through [`add_members`], the model's own write
    /// (the membership endpoints' `WHERE` clauses and follow rule are their
    /// ticket's business, and [`crate::api::lists`] tests them over HTTP).
    async fn add_member(pool: &PgPool, list: &List, member: &User) {
        let added = add_members(pool, list.id, &[member.id])
            .await
            .expect("the fixture membership adds");
        assert_eq!(added, 1, "exactly one membership went in");
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

    #[tokio::test]
    async fn lists_containing_answers_the_owners_membership_in_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn membership_is_idempotent_only_through_the_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn deleting_a_list_takes_its_memberships() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn deleting_a_user_takes_their_lists_and_memberships() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn deleting_a_member_takes_their_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn add_members_records_the_order_it_was_given() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &alice, "hers").await;

        let added = add_members(&pool, hers.id, &[carol.id, bob.id])
            .await
            .expect("both members add");
        assert_eq!(added, 2, "two memberships went in");
        assert_eq!(
            member_ids(&pool, hers.id).await.expect("members read"),
            vec![carol.id, bob.id],
            "and the list holds them in the order they were added, not the \
             order of their ids"
        );

        let positions = positions(&pool, hers.id).await;
        assert!(
            positions.first() < positions.get(1),
            "the earlier member has the smaller ordinal: {positions:?}"
        );
    }

    #[tokio::test]
    async fn adding_a_member_twice_writes_no_second_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;
        let before = positions(&pool, hers.id).await;

        let added = add_members(&pool, hers.id, &[bob.id, carol.id])
            .await
            .expect("adding a member twice is not a failure");
        assert_eq!(added, 1, "only the new account became a member");
        assert_eq!(
            member_ids(&pool, hers.id).await.expect("members read"),
            vec![bob.id, carol.id],
            "bob stayed, carol joined"
        );
        let after = positions(&pool, hers.id).await;
        assert_eq!(
            after.first(),
            before.first(),
            "and re-adding did not move bob to the end of the list"
        );
    }

    #[tokio::test]
    async fn adding_nobody_writes_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = seed(&pool, &alice, "hers").await;

        assert_eq!(
            add_members(&pool, hers.id, &[])
                .await
                .expect("an empty add succeeds"),
            0,
            "no id, no membership"
        );
        assert!(
            member_ids(&pool, hers.id)
                .await
                .expect("members read")
                .is_empty(),
            "and the list is still empty"
        );
    }

    #[tokio::test]
    async fn adding_to_a_list_that_does_not_exist_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = user(&pool, "bob").await;

        let err = add_members(&pool, Uuid::now_v7(), &[bob.id])
            .await
            .expect_err("no list row, no membership");
        assert!(
            matches!(err, AppError::Database(_)),
            "the list_id foreign key refuses it: {err}"
        );
        assert_eq!(count_members(&pool).await, 0, "and nothing was written");
    }

    #[tokio::test]
    async fn remove_members_takes_one_list_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = seed(&pool, &alice, "first").await;
        let second = seed(&pool, &alice, "second").await;
        add_member(&pool, &first, &bob).await;
        add_member(&pool, &second, &bob).await;

        let removed = remove_members(&pool, first.id, &[bob.id])
            .await
            .expect("the member goes");
        assert_eq!(removed, 1, "one membership ended");
        assert!(
            member_ids(&pool, first.id)
                .await
                .expect("members read")
                .is_empty(),
            "of the list it was named in"
        );
        assert_eq!(
            member_ids(&pool, second.id).await.expect("members read"),
            vec![bob.id],
            "while his other membership stands"
        );

        assert_eq!(
            remove_members(&pool, first.id, &[bob.id])
                .await
                .expect("removing a non-member succeeds"),
            0,
            "and removing him again removes nobody"
        );
    }

    #[tokio::test]
    async fn removing_nobody_takes_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &alice, "hers").await;

        for ids in [Vec::new(), vec![carol.id]] {
            assert_eq!(
                remove_members(&pool, hers.id, &ids)
                    .await
                    .expect("an empty or unmatched remove succeeds"),
                0,
                "{ids:?} named no member"
            );
        }
    }

    /// One page of `list`'s members with `query` applied, unwrapped with the
    /// caller's own reason `why`.
    async fn members(pool: &PgPool, list: &List, query: &str, why: &str) -> Vec<MemberEntry> {
        members_page(pool, list.id, ordinal_page(query))
            .await
            .unwrap_or_else(|err| panic!("{why}: {err}"))
    }

    /// The `position` of entry `index` of `entries` — the ordinal a cursor is
    /// keyed on — spelled as the page spells it.
    fn position_of(entries: &[MemberEntry], index: usize, why: &str) -> String {
        entries
            .get(index)
            .unwrap_or_else(|| panic!("{why}"))
            .position
            .to_string()
    }

    #[tokio::test]
    async fn members_page_is_newest_membership_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;
        add_member(&pool, &hers, &carol).await;
        add_member(&pool, &hers, &dave).await;
        let theirs = seed(&pool, &alice, "theirs").await;
        add_member(&pool, &theirs, &bob).await;

        let first = members(&pool, &hers, "limit=2", "the first page reads").await;
        assert_eq!(
            nicks(&first),
            ["dave", "carol"],
            "newest membership first, and only the named list's"
        );
        let oldest = position_of(&first, 1, "the page holds two entries");
        let newest = position_of(&first, 0, "the first page holds two entries");

        let second = members(
            &pool,
            &hers,
            &format!("limit=2&max_id={oldest}"),
            "the next page reads",
        )
        .await;
        assert_eq!(
            nicks(&second),
            ["bob"],
            "max_id walks to the older memberships"
        );
        let only = position_of(&second, 0, "the second page holds one entry");

        let back = members(
            &pool,
            &hers,
            &format!("limit=2&min_id={only}"),
            "and min_id walks back",
        )
        .await;
        assert_eq!(nicks(&back), ["dave", "carol"], "newer ones, newest first");

        let since_newest = members(
            &pool,
            &hers,
            &format!("limit=2&since_id={newest}"),
            "since_id reads unseen rows",
        )
        .await;
        assert!(
            since_newest.is_empty(),
            "nothing is newer than the newest membership the client has seen"
        );

        let since_oldest = members(
            &pool,
            &hers,
            &format!("limit=2&since_id={only}"),
            "and a cursor further back reads forward",
        )
        .await;
        assert_eq!(
            nicks(&since_oldest),
            ["dave", "carol"],
            "since_id answers everything newer than the cursor, newest first"
        );
    }

    #[tokio::test]
    async fn a_zero_limit_answers_every_member() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = seed(&pool, &alice, "hers").await;
        let mut added = Vec::new();
        for index in 0..25 {
            let member = user(&pool, &format!("member{index}")).await;
            add_member(&pool, &hers, &member).await;
            added.push(member);
        }
        assert_eq!(added.len(), 25, "twenty-five members, over a page");

        let capped = members_page(&pool, hers.id, ordinal_page(""))
            .await
            .expect("the default page reads");
        assert_eq!(capped.len(), 20, "and the default page is 20 of them");

        let every = members_page(&pool, hers.id, ordinal_page("limit=0"))
            .await
            .expect("limit=0 reads");
        assert_eq!(every.len(), 25, "and every member is on it");
        assert_eq!(
            every.first().expect("the page is not empty").user.id,
            added.last().expect("twenty-five members were added").id,
            "newest membership first"
        );
    }

    #[tokio::test]
    async fn members_of_an_empty_list_are_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = seed(&pool, &alice, "hers").await;

        assert!(
            members_page(&pool, hers.id, ordinal_page(""))
                .await
                .expect("reading an empty list succeeds")
                .is_empty(),
            "no memberships, no entries"
        );
        assert!(
            member_ids(&pool, hers.id)
                .await
                .expect("reading an empty list succeeds")
                .is_empty(),
            "and no ids"
        );
    }

    #[tokio::test]
    async fn member_ids_of_an_unknown_list_are_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            member_ids(&pool, Uuid::now_v7())
                .await
                .expect("reading nothing is not a failure")
                .is_empty(),
            "a list that never existed has no members"
        );
    }

    /// The `list_members.position` ordinals of `list_id`'s members, oldest
    /// first — what the page keys on.
    async fn positions(pool: &PgPool, list_id: Uuid) -> Vec<i64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT position FROM list_members WHERE list_id = $1 ORDER BY position",
        )
        .bind(list_id)
        .fetch_all(pool)
        .await
        .expect("the positions read")
    }

    /// Ordinal page params built from a query string, the way the endpoint's
    /// parse makes them.
    fn ordinal_page(query: &str) -> OrdinalPageParams {
        OrdinalPageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture query parses")
    }

    /// The nicknames of `entries`' members, in the order they came back.
    fn nicks(entries: &[MemberEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.user.nickname.as_str())
            .collect()
    }
}
