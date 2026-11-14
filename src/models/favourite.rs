//! The `favourites` table: one account favouriting one status — a `Like`,
//! relationally.
//!
//! Reference: Pleroma's `CommonAPI.favorite` / `unfavorite`
//! (`lib/pleroma/web/common_api.ex`), which write a `Like` activity per
//! favourite and an `Undo` per withdrawal and keep the roll-up in the
//! liked object's `likes` / `like_count` JSONB; here the fact is this
//! table (`docs/ARCHITECTURE.md` decision 3): one row per
//! `(user_id, status_id)` plus the stored `statuses.favourites_count`
//! [`favourite`] and [`unfavourite`] keep in step with it.
//!
//! Both writes are idempotent on that key — favouriting twice is one row
//! that keeps its id, its `ap_id` and its place in every page, and
//! withdrawing a favourite that is not there (or no longer is) removes
//! nothing — the same pair of moves [`crate::models::emoji_reaction`]
//! makes. The counter moves only when the row does: a repeat favourite
//! answers `None` and steps nothing, so the number can never drift past
//! the rows.
//!
//! What a favourite may *name* is the caller's gate: the endpoint resolves
//! the status through
//! [`status_for_viewer_or_404`](crate::api::helpers::status_for_viewer_or_404)
//! first, and — Mastodon's behaviour for a reblog — passes the *original's*
//! id, so the row (and the count) lands on the liked post while the client
//! gets the reblog entity back.
//!
//! The three reads match the three surfaces the entity and the endpoints
//! need, all batched (`docs/ARCHITECTURE.md` decision 13):
//! [`favourited_status_ids`] answers the viewer's `favourited` flag for a
//! whole page of statuses in one statement, [`users_for_status`] pages the
//! accounts behind one status's favourites for the `favourited_by`
//! endpoint, and [`page_for_user`] pages the statuses behind one account's
//! favourites for `GET /api/v1/favourites`. Both page reads key on the
//! *favourite row id* — the `UUIDv7` the client's cursors and `Link`
//! header walk, not the joined row's.
//!
//! Deleting a user or a status takes its favourites through the foreign
//! key's cascade. The [`Favourite::ap_id`] awaits the federation ticket
//! that sends the `Like` activity (decision 6); [`favourite`] mints a
//! local one.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, postgres::PgRow};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        status::{self, Counter, Status},
        user::User,
    },
    pagination::PageParams,
    util::id::new_id,
};

/// [`favourite`]: the write, idempotent on `(user_id, status_id)`. A pair
/// that already liked keeps its row and returns no row at all, which is
/// what tells the caller a step of the counter is owed: `RETURNING *`
/// answers exactly the insert that happened.
const FAVOURITE_SQL: &str = "INSERT INTO favourites (id, user_id, status_id, ap_id)
     VALUES ($1, $2, $3, $4)
ON CONFLICT (user_id, status_id) DO NOTHING
RETURNING *";

/// [`unfavourite`]: one account's like of one status; a pair that never
/// favourited (or already withdrew) deletes nothing, so withdrawal is
/// idempotent. `RETURNING *` hands the caller the row that went, which is
/// what the write hook reports and the counter step waits for.
const UNFAVOURITE_SQL: &str = "DELETE FROM favourites
      WHERE user_id = $1 AND status_id = $2
   RETURNING *";

/// [`favourited_status_ids`]: which of a batch of statuses the viewer
/// favourited. Only the ids that *have* a row come back; the caller's
/// flag is membership in that set.
const FLAGGED_IDS_SQL: &str = "SELECT status_id FROM favourites
     WHERE user_id = $1 AND status_id = ANY($2)";

/// [`users_for_status`]: the accounts that favourited one status, each
/// entry led by the `favourite_id` of the like behind it — the id the page
/// keys on. The text stops where its one placeholder belongs, because that
/// is where [`page_rows`] pushes its bind and the builder is what numbers
/// it `$1`; the keyset it then appends runs on the favourite id `f.id`.
const USERS_PAGE_SQL: &str = "SELECT f.id AS favourite_id, u.* FROM favourites f
    JOIN users u ON u.id = f.user_id
     WHERE f.status_id = ";

/// [`page_for_user`]: one account's favourites, the full [`Favourite`]
/// row leading — the id the page keys on, which the endpoint's `Link`
/// header carries rather than the status id. The joined statuses come from
/// [`status::get_many`] in a second read, so the keyset stays a plain walk
/// of the `(user_id, id desc)` pair the table's migration indexes.
const FAVOURITES_PAGE_SQL: &str = "SELECT f.* FROM favourites f
     WHERE f.user_id = ";

/// One row of `favourites`: `user_id` favourited `status_id`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Favourite {
    /// `UUIDv7` primary key, minted by [`favourite`] (decision 4); the
    /// cursor both page reads key on.
    pub id: Uuid,
    /// The favouriting account.
    pub user_id: Uuid,
    /// The favourited status (for a reblog, the original — see the
    /// [module docs](self)).
    pub status_id: Uuid,
    /// `ActivityPub` id of the `Like` activity behind this row
    /// (decision 6): a `{BASE_URL}/activities/{uuid}` id minted for a
    /// local favourite, to be replaced by the remote id for a mirrored
    /// one.
    pub ap_id: Option<String>,
    /// When the favourite was made; a repeat keeps this, since it keeps
    /// the row.
    pub inserted_at: DateTime<Utc>,
}

/// One entry of [`users_for_status`]: the [`User`] behind a favourite,
/// led by that favourite's row id.
///
/// The id is what the endpoint's keyset cursors and `Link` header carry,
/// exactly as [`crate::models::follow`] hands the relationship id over
/// next to the account.
#[derive(Debug, sqlx::FromRow)]
pub struct FavouritePageEntry {
    /// Primary key of the [`Favourite`] row behind this entry.
    pub favourite_id: Uuid,
    /// The account that favourited.
    #[sqlx(flatten)]
    pub user: User,
}

/// Records `user_id` favouriting `status_id` and returns the new row, or
/// `None` when the pair already liked the status.
///
/// The write is idempotent on the table's unique key: a pair that already
/// favourited keeps its row exactly as it is — same id, same `ap_id`, same
/// `inserted_at` — steps nothing, and answers `None`, so the caller can
/// tell a favourite from a restatement. Only the insert that happened
/// bumps `statuses.favourites_count` (through
/// [`status::bump_counter`], clamped at zero).
///
/// `base_url` is the instance origin, i.e. [`Config::base_url`], which the
/// model layer is handed rather than reading configuration itself; the
/// minted `ap_id` is a fresh `{base_url}/activities/{uuid}` for this row's
/// `Like` activity (decision 6), which the federation ticket later
/// delivers. `status_id` is the status the *like* lands on: for a reblog
/// the endpoint passes the original's id, never the reblog row's.
///
/// [`Config::base_url`]: crate::config::Config::base_url
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The two foreign keys
/// are the failures reachable here: an unknown `user_id` or `status_id`.
pub async fn favourite(
    pool: &PgPool,
    base_url: &str,
    user_id: Uuid,
    status_id: Uuid,
) -> AppResult<Option<Favourite>> {
    let created = sqlx::query_as::<_, Favourite>(FAVOURITE_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(status_id)
        .bind(local_activity_id(base_url))
        .fetch_optional(pool)
        .await?;
    if created.is_some() {
        status::bump_counter(pool, status_id, Counter::Favourites, 1).await?;
    }
    Ok(created)
}

/// Removes `user_id`'s favourite of `status_id`, reporting the row that
/// went and stepping `statuses.favourites_count` down for it.
///
/// Withdrawing a favourite that is not there — never made, or already
/// withdrawn — removes nothing, steps nothing and is not an error, so the
/// write is idempotent; the answer is `None` either way and the caller
/// tells a withdrawal from a no-op by it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unfavourite(
    pool: &PgPool,
    user_id: Uuid,
    status_id: Uuid,
) -> AppResult<Option<Favourite>> {
    let removed = sqlx::query_as::<_, Favourite>(UNFAVOURITE_SQL)
        .bind(user_id)
        .bind(status_id)
        .fetch_optional(pool)
        .await?;
    if removed.is_some() {
        status::bump_counter(pool, status_id, Counter::Favourites, -1).await?;
    }
    Ok(removed)
}

/// The `SELECT` behind [`get`]: one account's like of one status. The C2S
/// outbox reads a repeat `Like`'s row through it to name the existing
/// activity in its `Location`.
const FAVOURITE_GET_SQL: &str = "SELECT * FROM favourites WHERE user_id = $1 AND status_id = $2";

/// The `SELECT` behind [`get_by_ap_id`]: the like of one account whose
/// `Like` activity carries one id (decision 6: a local like's `ap_id` is
/// `{BASE_URL}/activities/{uuid}`). Scoping the lookup to the favouriting
/// account keeps somebody else's activity id unfindable here, so an
/// `Undo` can only withdraw its own maker's like.
const FAVOURITE_BY_AP_ID_SQL: &str = "SELECT * FROM favourites WHERE user_id = $1 AND ap_id = $2";

/// The [`Favourite`] of (`user_id`, `status_id`), `None` when the pair has
/// no like row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, user_id: Uuid, status_id: Uuid) -> AppResult<Option<Favourite>> {
    let favourite = sqlx::query_as::<_, Favourite>(FAVOURITE_GET_SQL)
        .bind(user_id)
        .bind(status_id)
        .fetch_optional(pool)
        .await?;
    Ok(favourite)
}

/// The [`Favourite`] of `user_id` whose `Like` activity carries `ap_id`,
/// `None` when the account has no such like.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_ap_id(
    pool: &PgPool,
    user_id: Uuid,
    ap_id: &str,
) -> AppResult<Option<Favourite>> {
    let favourite = sqlx::query_as::<_, Favourite>(FAVOURITE_BY_AP_ID_SQL)
        .bind(user_id)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(favourite)
}

/// The `DELETE` behind [`delete_all_for_user`]: the account's own likes.
const DELETE_ALL_FOR_USER_SQL: &str = "DELETE FROM favourites WHERE user_id = $1";

/// Removes every favourite made by user `user_id`: the favourite step of
/// the [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// Favourites *of* the account's statuses are other accounts' rows and
/// stay; their targets are tombstones by the time this runs, and every
/// favourites read filters those out (`src/models/status_visibility.rs`).
///
/// Returns how many rows went.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let deleted = sqlx::query(DELETE_ALL_FOR_USER_SQL)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// Which of `status_ids` `user_id` has favourited — the batched `favourited`
/// read the status renderer runs once per page (`docs/ARCHITECTURE.md`
/// decision 13).
///
/// The answer is the membership set, not a per-status flag list: an id
/// with no row is simply absent, and an empty `status_ids` asks the
/// database nothing at all. The caller keys a reblog row's flag to its
/// *original's* id before consulting the set, because that is where the
/// like lives.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn favourited_status_ids(
    pool: &PgPool,
    user_id: Uuid,
    status_ids: &[Uuid],
) -> AppResult<HashSet<Uuid>> {
    if status_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query_as::<_, (Uuid,)>(FLAGGED_IDS_SQL)
        .bind(user_id)
        .bind(status_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// The accounts that favourited `status_id`, newest favourite first and
/// `page`'s `limit` at a time, each entry carrying the [`Favourite`] row's
/// id next to the [`User`] (see [`FavouritePageEntry`]).
///
/// This is the read behind `GET /api/v1/statuses/{id}/favourited_by`
/// (Pleroma's `favourited_by`, which walks the object's `likes` `ap_id`
/// list): the page keys on the *favourite* id, so the endpoint's
/// `max_id`/`min_id`/`since_id` cursors and its `Link` header carry like
/// row ids, not account ids. Who may see the list is the endpoint's gate
/// (the shared status visibility check), not this query's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn users_for_status(
    pool: &PgPool,
    status_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<FavouritePageEntry>> {
    page_rows(pool, USERS_PAGE_SQL, status_id, &page).await
}

/// `user_id`'s favourited statuses, newest favourite first and `page`'s
/// `limit` at a time, each entry a [`Favourite`] paired with the
/// [`Status`] it likes.
///
/// This is the read behind `GET /api/v1/favourites`: the page keys on the
/// favourite id (its cursors and `Link` header walk likes, not statuses),
/// and the pairs come back in that order. The two reads — the keyset over
/// `favourites`, then one [`status::get_many`] for the page — are what
/// keeps this a fixed pair of statements whatever the page. A favourite
/// whose status no longer answers — hard-deleted, or a tombstone, which
/// [`status::get_many`] excludes — contributes nothing: a like of a gone
/// post is on no page. Whether the viewer may still *see* the statuses
/// that do come back is the endpoint's [`filter_visible`] step, not this
/// one's.
///
/// [`filter_visible`]: crate::models::status_visibility::filter_visible
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<(Favourite, Status)>> {
    let favourites: Vec<Favourite> = page_rows(pool, FAVOURITES_PAGE_SQL, user_id, &page).await?;
    let status_ids: Vec<Uuid> = favourites.iter().map(|row| row.status_id).collect();
    let live = status::get_many(pool, &status_ids).await?;
    let mut by_id: HashMap<Uuid, Status> = live.into_iter().map(|row| (row.id, row)).collect();
    Ok(favourites
        .into_iter()
        .filter_map(|favourite| {
            let status = by_id.remove(&favourite.status_id)?;
            Some((favourite, status))
        })
        .collect())
}

/// One keyset page of `sql` with `bound_id` as its leading placeholder:
/// [`users_for_status`] and [`page_for_user`] share the builder walk, the
/// same shape [`crate::models::follow`] uses — the statement text stops at
/// the bind, the keyset runs on the favourite id `f.id`, and a `min_id`
/// page comes back flipped to newest-first.
async fn page_rows<T>(
    pool: &PgPool,
    sql: &str,
    bound_id: Uuid,
    page: &PageParams,
) -> AppResult<Vec<T>>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    let mut builder = QueryBuilder::<Postgres>::new(sql);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(bound_id);
    page.apply_keyset(&mut builder, "f.id");
    let rows = builder.build_query_as::<T>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

/// The `ap_id` [`favourite`] stores: a freshly minted
/// `{base_url}/activities/{uuid}` (decision 6). Every favourite this model
/// writes comes from a local account through the API; the federation
/// ticket that mirrors a remote `Like` records that activity's own id.
fn local_activity_id(base_url: &str) -> String {
    format!("{base_url}/activities/{}", new_id())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            status::{NewStatus, insert, soft_delete},
            user::{NewUser, User, insert as insert_user, local_ap_id},
        },
        pagination::MAX_LIMIT,
    };

    /// The instance origin the fixtures favourite on.
    const BASE: &str = TEST_BASE_URL;
    /// The activity-id prefix [`favourite`] mints.
    const ACTIVITY_PREFIX: &str = "/activities/";
    /// How many favourites the page tests seed.
    const PAGE_SEED: usize = 3;

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A public post by `author`, as its row.
    async fn post(pool: &PgPool, author: &User) -> Status {
        insert(pool, BASE, &NewStatus::local(author, "hello"))
            .await
            .expect("the fixture post inserts")
    }

    /// A public post by a fresh local author named `nickname`, as its id.
    async fn post_by(pool: &PgPool, nickname: &str) -> Uuid {
        post(pool, &user(pool, nickname).await).await.id
    }

    /// Favourites `status_id` as `who`, asserting the write created a row
    /// (a test that needs the no-op arm calls [`favourite`] itself).
    async fn favourite_as(pool: &PgPool, who: &User, status_id: Uuid) {
        let created = favourite(pool, BASE, who.id, status_id)
            .await
            .expect("the fixture favourite stores");
        assert!(created.is_some(), "the fixture pair had not favourited yet");
    }

    /// Every stored favourite as `(id, user_id, status_id)`, newest id
    /// first — the order the pages walk — so an assertion says what the
    /// table holds rather than what a loader chose to show.
    async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid, Uuid)> {
        sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
            "SELECT id, user_id, status_id FROM favourites ORDER BY id DESC",
        )
        .fetch_all(pool)
        .await
        .expect("the table reads")
    }

    /// The stored `favourites_count` of status `status_id`.
    async fn count(pool: &PgPool, status_id: Uuid) -> i32 {
        let row: (i32,) = sqlx::query_as("SELECT favourites_count FROM statuses WHERE id = $1")
            .bind(status_id)
            .fetch_one(pool)
            .await
            .expect("the counter reads");
        row.0
    }

    /// Page params built from a query string, the way the extractor makes
    /// them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    /// The ids of `entries`.
    fn entry_ids(entries: &[FavouritePageEntry]) -> Vec<Uuid> {
        entries.iter().map(|entry| entry.favourite_id).collect()
    }

    /// The favourite ids of `pairs`, in order.
    fn pair_ids(pairs: &[(Favourite, Status)]) -> Vec<Uuid> {
        pairs.iter().map(|(favourite, _)| favourite.id).collect()
    }

    #[tokio::test]
    async fn favouriting_writes_the_row_and_bumps_the_counter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;

        let created = favourite(&pool, BASE, alice.id, status_id)
            .await
            .expect("the first favourite stores")
            .expect("the pair had nothing to keep");

        assert_eq!(created.user_id, alice.id, "who favourited");
        assert_eq!(created.status_id, status_id, "what they favourited");
        let ap_id = created
            .ap_id
            .clone()
            .expect("a local favourite mints an activity id");
        let activity = ap_id
            .strip_prefix(&format!("{BASE}{ACTIVITY_PREFIX}"))
            .unwrap_or_default();
        assert!(
            crate::util::id::parse_id(activity).is_some(),
            "the id is this instance's origin, `/activities/` and a uuid, \
             not {ap_id}"
        );
        assert_eq!(
            stored(&pool).await,
            vec![(created.id, alice.id, status_id)],
            "one row"
        );
        assert_eq!(count(&pool, status_id).await, 1, "and one count");
    }

    #[tokio::test]
    async fn favouriting_twice_is_one_row_and_one_count() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        let first = favourite(&pool, BASE, alice.id, status_id)
            .await
            .expect("the first favourite stores")
            .expect("the pair had nothing to keep");

        let again = favourite(&pool, BASE, alice.id, status_id)
            .await
            .expect("repeating a favourite is not an error");

        assert!(again.is_none(), "the repeat created no row and no count");
        assert_eq!(
            stored(&pool).await,
            vec![(first.id, alice.id, status_id)],
            "still one row in the table"
        );
        assert_eq!(count(&pool, status_id).await, 1, "the count stayed one");
    }

    #[tokio::test]
    async fn unfavouriting_removes_the_row_and_steps_the_counter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        favourite_as(&pool, &alice, status_id).await;

        let removed = unfavourite(&pool, alice.id, status_id)
            .await
            .expect("the withdrawal runs")
            .expect("the favourite was there");
        assert_eq!(removed.user_id, alice.id, "the row that went");
        assert_eq!(removed.status_id, status_id, "and its status");
        assert_eq!(count(&pool, status_id).await, 0, "the count steps down");

        assert!(
            unfavourite(&pool, alice.id, status_id)
                .await
                .expect("repeating a withdrawal runs")
                .is_none(),
            "withdrawing what is already gone removes nothing"
        );
        assert!(stored(&pool).await.is_empty(), "the table is empty again");
        assert_eq!(
            count(&pool, status_id).await,
            0,
            "and a no-op steps nothing, never below zero either"
        );
    }

    #[tokio::test]
    async fn favourites_are_per_account_and_per_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let first = post(&pool, &carol).await;
        let second = post(&pool, &carol).await;

        favourite_as(&pool, &alice, first.id).await;
        favourite_as(&pool, &bob, first.id).await;
        favourite_as(&pool, &alice, second.id).await;

        let flagged = favourited_status_ids(&pool, alice.id, &[first.id, second.id])
            .await
            .expect("the flags read");
        assert!(flagged.contains(&first.id), "alice liked the first");
        assert!(flagged.contains(&second.id), "and the second");
        assert!(
            !favourited_status_ids(&pool, bob.id, &[first.id, second.id])
                .await
                .expect("bob's flags read")
                .contains(&second.id),
            "but bob only liked the first"
        );
        assert_eq!(
            favourited_status_ids(&pool, alice.id, &[Uuid::now_v7()])
                .await
                .expect("an unliked id reads"),
            HashSet::new(),
            "an id with no row is simply absent"
        );
        assert_eq!(
            count(&pool, first.id).await,
            2,
            "the first post carries both likes"
        );
        assert_eq!(count(&pool, second.id).await, 1, "the second only alice's");
    }

    #[tokio::test]
    async fn the_users_of_a_status_page_over_the_like_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let status_id = post_by(&pool, "author").await;
        let mut likers = Vec::new();
        for n in 0..PAGE_SEED {
            let liker = user(&pool, &format!("liker{n}")).await;
            favourite_as(&pool, &liker, status_id).await;
            likers.push(liker);
        }
        let all = stored(&pool).await;
        assert_eq!(all.len(), PAGE_SEED, "three fixture favourites");

        let first_page = users_for_status(&pool, status_id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(
            entry_ids(&first_page),
            vec![all[0].0, all[1].0],
            "the two newest likes"
        );
        assert_eq!(
            first_page[0].user.id, all[0].1,
            "and each entry carries the account behind its like"
        );

        let older = users_for_status(&pool, status_id, page(&format!("max_id={}", all[1].0)))
            .await
            .expect("the continuation reads");
        assert_eq!(
            entry_ids(&older),
            vec![all[2].0],
            "max_id continues where the page ended"
        );
        assert!(
            users_for_status(&pool, status_id, page(&format!("max_id={}", all[2].0)))
                .await
                .expect("the walk off the end reads")
                .is_empty(),
            "and past the oldest like there is nothing"
        );

        let newer = users_for_status(&pool, status_id, page(&format!("min_id={}", all[2].0)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            entry_ids(&newer),
            vec![all[0].0, all[1].0],
            "min_id answers the newer page, newest-first after the flip"
        );

        assert!(
            users_for_status(&pool, post_by(&pool, "quiet").await, page(""))
                .await
                .expect("an unliked status reads")
                .is_empty(),
            "a status nobody favourited answers nothing"
        );
        assert_eq!(
            likers.len(),
            PAGE_SEED,
            "the fixture seeded three distinct accounts, one like each"
        );
    }

    #[tokio::test]
    async fn a_user_page_pairs_each_like_with_its_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = post(&pool, &bob).await;
        let second = post(&pool, &bob).await;
        let third = post(&pool, &bob).await;
        favourite_as(&pool, &alice, first.id).await;
        favourite_as(&pool, &alice, second.id).await;
        favourite_as(&pool, &alice, third.id).await;
        let carol = user(&pool, "carol").await;
        favourite_as(&pool, &carol, first.id).await;

        let all = sqlx::query_as::<_, (Uuid,)>(
            "SELECT id FROM favourites WHERE user_id = $1 ORDER BY id DESC",
        )
        .bind(alice.id)
        .fetch_all(&pool)
        .await
        .expect("alice's rows read");
        let alice_ids: Vec<Uuid> = all.into_iter().map(|(id,)| id).collect();

        let pairs = page_for_user(&pool, alice.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(
            pair_ids(&pairs),
            vec![alice_ids[0], alice_ids[1]],
            "newest favourite first, keyed on like ids"
        );
        let statuses: HashSet<Uuid> = [first.id, second.id, third.id].into_iter().collect();
        assert!(
            pairs
                .iter()
                .all(|(favourite, status)| statuses.contains(&status.id)
                    && favourite.status_id == status.id),
            "every entry pairs its like with the status it likes"
        );

        let older = page_for_user(&pool, alice.id, page(&format!("max_id={}", alice_ids[1])))
            .await
            .expect("the continuation reads");
        assert_eq!(
            pair_ids(&older),
            vec![alice_ids[2]],
            "max_id walks alice's own like ids"
        );
        assert_eq!(
            older[0].0.user_id, alice.id,
            "carol's like of the same status is on no page of alice's"
        );

        assert!(
            page_for_user(&pool, user(&pool, "lonely").await.id, page(""))
                .await
                .expect("an account with no favourites reads")
                .is_empty(),
            "nobody's list is the empty page"
        );
    }

    #[tokio::test]
    async fn a_pair_leaves_the_page_when_its_status_goes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        for _ in 0..PAGE_SEED {
            let liked = post(&pool, &bob).await;
            favourite_as(&pool, &alice, liked.id).await;
        }
        let full = page_for_user(&pool, alice.id, page(""))
            .await
            .expect("the whole list reads");
        assert_eq!(full.len(), PAGE_SEED, "one pair per favourite");

        // A hard-deleted status drops its whole pair from the page (not
        // the tombstone the endpoint's visibility step handles): a like of
        // a gone post is on no page.
        let gone = full[1].1.id;
        let erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(gone)
            .execute(&pool)
            .await
            .expect("the fixture status deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the status was there to delete");
        let after = page_for_user(&pool, alice.id, page(""))
            .await
            .expect("the page after the delete reads");
        assert_eq!(
            after
                .iter()
                .map(|(_, status)| status.id)
                .collect::<Vec<Uuid>>(),
            vec![full[0].1.id, full[2].1.id],
            "the gone status contributes nothing and the survivors keep \
             the like-id order"
        );
        assert!(
            after
                .iter()
                .all(|(favourite, status)| status.id == favourite.status_id),
            "and every remaining pair still agrees"
        );
    }

    #[tokio::test]
    async fn tombstones_drop_out_of_pages_and_deletes_cascade() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let liked = post_by(&pool, "bob").await;
        let kept = post_by(&pool, "carol").await;
        favourite_as(&pool, &alice, liked).await;
        favourite_as(&pool, &alice, kept).await;

        // A soft delete leaves the row (and the like) in place: the like
        // survives, the count stays, and only the page read drops the
        // tombstone — while the visibility gate is the endpoint's step.
        let tombstone = soft_delete(&pool, liked)
            .await
            .expect("the delete runs")
            .expect("the status was live");
        assert!(tombstone.deleted_at.is_some(), "the row is a tombstone");
        assert_eq!(
            count(&pool, liked).await,
            1,
            "soft_delete counts nothing back"
        );
        let pairs = page_for_user(&pool, alice.id, page(""))
            .await
            .expect("the page reads");
        assert_eq!(pairs.len(), 1, "the tombstoned like is off the page");
        assert_eq!(pairs[0].1.id, kept, "and the kept one stays on it");

        // A hard delete takes the like rows with it through the cascade.
        let erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(kept)
            .execute(&pool)
            .await
            .expect("the fixture status deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the kept post was there to delete");
        let remaining = stored(&pool).await;
        assert_eq!(
            remaining
                .iter()
                .map(|(_, _, status_id)| *status_id)
                .collect::<Vec<Uuid>>(),
            vec![liked],
            "only the tombstone's like remains — a hard-deleted status \
             takes its likes with it"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_cascades_their_favourites() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        favourite_as(&pool, &alice, status_id).await;

        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the favouriter was there to delete");
        assert!(
            stored(&pool).await.is_empty(),
            "and the cascade emptied the table"
        );
    }

    #[tokio::test]
    async fn an_unknown_status_refuses_the_write() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        let err = favourite(&pool, BASE, alice.id, Uuid::now_v7())
            .await
            .expect_err("a status that does not exist cannot be favourited");
        assert!(
            matches!(err, AppError::Database(_)),
            "the foreign key refuses, not a silent no-op: {err}"
        );
    }

    /// The two new reads beside the write: [`get`] finds the pair's row
    /// only once it exists, and [`get_by_ap_id`] finds it by the
    /// `{BASE_URL}/activities/{uuid}` id [`favourite`] minted — and finds
    /// nothing for another account quoting the same id, which is what
    /// keeps a C2S `Undo` inside its maker's facts.
    #[tokio::test]
    async fn get_reads_the_pair_and_get_by_ap_id_the_activity_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let carol = user(&pool, "carol").await;
        let status_id = post_by(&pool, "bob").await;

        assert!(
            get(&pool, alice.id, status_id)
                .await
                .expect("the pair read runs")
                .is_none(),
            "no like yet, no row"
        );

        let created = favourite(&pool, BASE, alice.id, status_id)
            .await
            .expect("the favourite stores")
            .expect("the pair had not favourited yet");
        let found = get(&pool, alice.id, status_id)
            .await
            .expect("the pair read runs")
            .expect("the write left a row");
        assert_eq!(found.id, created.id, "the pair read finds the row");

        let ap_id = created.ap_id.expect("a local like carries an activity id");
        assert!(ap_id.starts_with(&format!("{BASE}/activities/")));
        let by_id = get_by_ap_id(&pool, alice.id, &ap_id)
            .await
            .expect("the activity-id read runs")
            .expect("the id names the row");
        assert_eq!(by_id.id, created.id, "the id read finds the same row");
        assert!(
            get_by_ap_id(&pool, carol.id, &ap_id)
                .await
                .expect("the activity-id read runs")
                .is_none(),
            "and another account quoting it names none of its own"
        );
    }
}
