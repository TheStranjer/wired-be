//! The `bookmarks` table: one account bookmarking one status — a private
//! fact, relationally.
//!
//! Reference: Pleroma's `bookmark`, `unbookmark` and `bookmarks` actions
//! (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`) over
//! `Pleroma.Bookmark` (`lib/pleroma/bookmark.ex`), which store a `Bookmark`
//! row per `(account, activity)`; here the fact is this table
//! (`docs/ARCHITECTURE.md` decision 3): one row per `(user_id, status_id)`.
//! Nothing federates: a bookmark is the account's own note-to-self, Pleroma
//! sends no activity for it, and so — unlike [`crate::models::favourite`] —
//! the row carries no `ap_id` and there is no stored counter to keep in
//! step.
//!
//! Both writes are idempotent on the unique key, the same pair of moves
//! [`crate::models::emoji_reaction`] and [`crate::models::favourite`]
//! make: bookmarking twice is one row that keeps its id and its place in
//! every page, and removing a bookmark that is not there (or no longer is)
//! deletes nothing.
//!
//! What a bookmark may *name* is the caller's gate: the endpoint resolves
//! the status through
//! [`status_for_viewer_or_404`](crate::api::helpers::status_for_viewer_or_404)
//! first. Unlike a favourite, a bookmark of a reblog id lands on *the
//! reblog row itself* — Pleroma's `Bookmark.create` takes the activity the
//! client named, and nothing remaps it to the original.
//!
//! The two reads match the two surfaces the entity and the endpoints need,
//! both batched (`docs/ARCHITECTURE.md` decision 13):
//! [`bookmarked_ids`] answers the viewer's `bookmarked` flag for a whole
//! page of statuses in one statement, and [`page_for_user`] pages the
//! statuses behind one account's bookmarks for `GET /api/v1/bookmarks`.
//! Both key on the *bookmark row id* — the `UUIDv7` the client's cursors
//! and `Link` header walk, not the joined row's.
//!
//! [`Bookmark::folder_id`] awaits the folders ticket: until that migration
//! adds the foreign key nothing stores a folder here, and [`page_for_user`]
//! filters on the id it is given so the endpoint can wire the `folder_id`
//! query parameter straight in.
//!
//! Deleting a user or a status takes its bookmarks through the foreign
//! key's cascade.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::status::{self, Status},
    pagination::PageParams,
    util::id::new_id,
};

/// [`add`]: the write, idempotent on `(user_id, status_id)`. A pair that
/// already bookmarked keeps its row and returns no row at all —
/// `RETURNING *` answers exactly the insert that happened, which is what
/// tells the caller a fresh bookmark landed.
const ADD_SQL: &str = "INSERT INTO bookmarks (id, user_id, status_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, status_id) DO NOTHING
RETURNING *";

/// [`remove`]: one account's bookmark of one status; a pair that never
/// bookmarked (or already withdrew) deletes nothing, so withdrawal is
/// idempotent. `RETURNING *` hands the caller the row that went.
const REMOVE_SQL: &str = "DELETE FROM bookmarks
      WHERE user_id = $1 AND status_id = $2
   RETURNING *";

/// [`bookmarked_ids`]: which of a batch of statuses the viewer bookmarked.
/// Only the ids that *have* a row come back; the caller's flag is
/// membership in that set.
const FLAGGED_IDS_SQL: &str = "SELECT status_id FROM bookmarks
     WHERE user_id = $1 AND status_id = ANY($2)";

/// [`page_for_user`]: one account's bookmarks, the full [`Bookmark`] row
/// leading — the id the page keys on, which the endpoint's `Link` header
/// carries rather than the status id. The joined statuses come from
/// [`status::get_many`] in a second read, so the keyset stays a plain walk
/// of the `(user_id, id desc)` pair the table's migration indexes. The
/// text stops where its one placeholder belongs, because that is where
/// [`page_rows`] pushes its bind and the builder is what numbers it `$1`;
/// the folder clause and the keyset it then append run on `b.folder_id`
/// and the bookmark id `b.id`.
const BOOKMARKS_PAGE_SQL: &str = "SELECT b.* FROM bookmarks b
     WHERE b.user_id = ";

/// One row of `bookmarks`: `user_id` bookmarked `status_id`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Bookmark {
    /// `UUIDv7` primary key, minted by [`add`] (decision 4); the cursor
    /// both page reads key on.
    pub id: Uuid,
    /// The bookmarking account.
    pub user_id: Uuid,
    /// The bookmarked status (for a reblog id, the reblog row itself — see
    /// the [module docs](self)).
    pub status_id: Uuid,
    /// The bookmark's folder, if it is filed in one. `NULL` until the
    /// folders ticket creates `bookmark_folders` and wires the writes;
    /// this instance's clients cannot name a folder before then.
    pub folder_id: Option<Uuid>,
    /// When the bookmark was made; a repeat keeps this, since it keeps the
    /// row.
    pub inserted_at: DateTime<Utc>,
}

/// Records `user_id` bookmarking `status_id` and returns the new row, or
/// `None` when the pair already held the bookmark.
///
/// The write is idempotent on the table's unique key: a pair that already
/// bookmarked keeps its row exactly as it is — same id, same `inserted_at`
/// — and answers `None`, so the caller can tell a bookmark from a
/// restatement. Nothing else moves: a bookmark has no stored counter and
/// federates nothing.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The two foreign keys
/// are the failures reachable here: an unknown `user_id` or `status_id`.
pub async fn add(pool: &PgPool, user_id: Uuid, status_id: Uuid) -> AppResult<Option<Bookmark>> {
    Ok(sqlx::query_as::<_, Bookmark>(ADD_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(status_id)
        .fetch_optional(pool)
        .await?)
}

/// Removes `user_id`'s bookmark of `status_id`, reporting the row that
/// went.
///
/// Withdrawing a bookmark that is not there — never made, or already
/// withdrawn — removes nothing and is not an error, so the write is
/// idempotent; the answer is `None` either way and the caller tells a
/// withdrawal from a no-op by it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remove(pool: &PgPool, user_id: Uuid, status_id: Uuid) -> AppResult<Option<Bookmark>> {
    Ok(sqlx::query_as::<_, Bookmark>(REMOVE_SQL)
        .bind(user_id)
        .bind(status_id)
        .fetch_optional(pool)
        .await?)
}

/// The `DELETE` behind [`delete_all_for_user`]: the account's own marks.
const DELETE_ALL_FOR_USER_SQL: &str = "DELETE FROM bookmarks WHERE user_id = $1";

/// Removes every bookmark made by user `user_id`: the bookmark step of the
/// [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// Bookmarks are never another account's record of this one (a bookmark
/// points at a status, not at its author's row), so the account's own rows
/// are all there is to take, and no counter follows them.
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

/// Which of `status_ids` `user_id` has bookmarked — the batched
/// `bookmarked` read the status renderer runs once per page
/// (`docs/ARCHITECTURE.md` decision 13).
///
/// The answer is the membership set, not a per-status flag list: an id
/// with no row is simply absent, and an empty `status_ids` asks the
/// database nothing at all. Unlike the favourites flag, a reblog row's
/// bookmark is keyed to *its own* id — the row the client named is where
/// [`add`] stored it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn bookmarked_ids(
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

/// `user_id`'s bookmarked statuses, newest bookmark first and `page`'s
/// `limit` at a time, each entry a [`Bookmark`] paired with the [`Status`]
/// it names.
///
/// This is the read behind `GET /api/v1/bookmarks`: the page keys on the
/// bookmark id (its cursors and `Link` header walk bookmark rows, not
/// statuses), and the pairs come back in that order. The two reads — the
/// keyset over `bookmarks`, then one [`status::get_many`] for the page —
/// are what keeps this a fixed pair of statements whatever the page. A
/// bookmark whose status no longer answers — hard-deleted, or a tombstone,
/// which [`status::get_many`] excludes — contributes nothing: a bookmark of
/// a gone post is on no page. Whether the viewer may still *see* the
/// statuses that do come back is the endpoint's [`filter_visible`] step,
/// not this one's.
///
/// `folder_id` narrows the walk to one folder's bookmarks; `None` asks for
/// all of them. Until the folders ticket stores folder ids the narrow arm
/// answers the empty page for any folder it names, and the endpoint hands
/// `None` whatever its caller asks for.
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
    folder_id: Option<Uuid>,
) -> AppResult<Vec<(Bookmark, Status)>> {
    let bookmarks = page_rows(pool, user_id, folder_id, &page).await?;
    let status_ids: Vec<Uuid> = bookmarks.iter().map(|row| row.status_id).collect();
    let live = status::get_many(pool, &status_ids).await?;
    let mut by_id: HashMap<Uuid, Status> = live.into_iter().map(|row| (row.id, row)).collect();
    Ok(bookmarks
        .into_iter()
        .filter_map(|bookmark| {
            let status = by_id.remove(&bookmark.status_id)?;
            Some((bookmark, status))
        })
        .collect())
}

/// One keyset page of `user_id`'s bookmarks, optionally narrowed to
/// `folder_id`: the same builder walk [`crate::models::favourite`] uses —
/// the statement text stops at the bind, the keyset runs on the bookmark
/// id `b.id`, and a `min_id` page comes back flipped to newest-first.
async fn page_rows(
    pool: &PgPool,
    user_id: Uuid,
    folder_id: Option<Uuid>,
    page: &PageParams,
) -> AppResult<Vec<Bookmark>> {
    let mut builder = QueryBuilder::<Postgres>::new(BOOKMARKS_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    if let Some(folder) = folder_id {
        let _narrowed: &mut _ = builder.push(" AND b.folder_id = ").push_bind(folder);
    }
    page.apply_keyset(&mut builder, "b.id");
    let rows = builder.build_query_as::<Bookmark>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::status::{NewStatus, insert, soft_delete},
        models::user::{NewUser, User, insert as insert_user, local_ap_id},
        pagination::MAX_LIMIT,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;
    /// How many bookmarks the page tests seed.
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

    /// Bookmarks `status_id` as `who`, asserting the write created a row
    /// (a test that needs the no-op arm calls [`add`] itself).
    async fn bookmark_as(pool: &PgPool, who: &User, status_id: Uuid) {
        let created = add(pool, who.id, status_id)
            .await
            .expect("the fixture bookmark stores");
        assert!(created.is_some(), "the fixture pair had not bookmarked yet");
    }

    /// Every stored bookmark as `(id, user_id, status_id)`, newest id
    /// first — the order the pages walk — so an assertion says what the
    /// table holds rather than what a loader chose to show.
    async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid, Uuid)> {
        sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
            "SELECT id, user_id, status_id FROM bookmarks ORDER BY id DESC",
        )
        .fetch_all(pool)
        .await
        .expect("the table reads")
    }

    /// Files `bookmark_id` into `folder`, the way the folders ticket's
    /// write will: until that migration adds the foreign key the column
    /// holds any uuid, and only the model's narrow read interprets it.
    async fn file_in_folder(pool: &PgPool, bookmark_id: Uuid, folder: Uuid) {
        let filed = sqlx::query("UPDATE bookmarks SET folder_id = $2 WHERE id = $1")
            .bind(bookmark_id)
            .bind(folder)
            .execute(pool)
            .await
            .expect("the fixture filing runs")
            .rows_affected();
        assert_eq!(filed, 1, "the bookmark row was there to file");
    }

    /// Page params built from a query string, the way the extractor makes
    /// them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    /// The bookmark ids of `pairs`, in order.
    fn pair_ids(pairs: &[(Bookmark, Status)]) -> Vec<Uuid> {
        pairs.iter().map(|(bookmark, _)| bookmark.id).collect()
    }

    /// The status ids of `pairs`, in order.
    fn status_ids(pairs: &[(Bookmark, Status)]) -> Vec<Uuid> {
        pairs.iter().map(|(_, status)| status.id).collect()
    }

    /// All of `user_id`'s bookmark ids, newest first.
    async fn stored_ids(pool: &PgPool, user_id: Uuid) -> Vec<Uuid> {
        sqlx::query_as::<_, (Uuid,)>("SELECT id FROM bookmarks WHERE user_id = $1 ORDER BY id DESC")
            .bind(user_id)
            .fetch_all(pool)
            .await
            .expect("the ids read")
            .into_iter()
            .map(|(id,)| id)
            .collect()
    }

    #[tokio::test]
    async fn adding_writes_the_row_and_repeating_is_one_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;

        let created = add(&pool, alice.id, status_id)
            .await
            .expect("the first bookmark stores")
            .expect("the pair had nothing to keep");

        assert_eq!(created.user_id, alice.id, "who bookmarked");
        assert_eq!(created.status_id, status_id, "what they bookmarked");
        assert_eq!(
            created.folder_id, None,
            "nothing files a bookmark until the folders ticket"
        );
        assert!(
            created.id.get_version_num() == 7,
            "the id is a uuidv7 (decision 4)"
        );

        let again = add(&pool, alice.id, status_id)
            .await
            .expect("repeating a bookmark is not an error");

        assert!(again.is_none(), "the repeat created no row");
        assert_eq!(
            stored(&pool).await,
            vec![(created.id, alice.id, status_id)],
            "still one row in the table, and it kept its id"
        );
    }

    #[tokio::test]
    async fn removing_takes_the_row_and_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        let created = add(&pool, alice.id, status_id)
            .await
            .expect("the fixture bookmark stores")
            .expect("the pair had nothing to keep");

        let removed = remove(&pool, alice.id, status_id)
            .await
            .expect("the withdrawal runs")
            .expect("the bookmark was there");
        assert_eq!(removed.id, created.id, "the row that went");

        assert!(
            remove(&pool, alice.id, status_id)
                .await
                .expect("repeating a withdrawal runs")
                .is_none(),
            "withdrawing what is already gone removes nothing"
        );
        assert!(stored(&pool).await.is_empty(), "the table is empty again");
    }

    #[tokio::test]
    async fn bookmarks_are_per_account_and_per_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let first = post(&pool, &carol).await;
        let second = post(&pool, &carol).await;

        bookmark_as(&pool, &alice, first.id).await;
        bookmark_as(&pool, &bob, first.id).await;
        bookmark_as(&pool, &alice, second.id).await;

        let flagged = bookmarked_ids(&pool, alice.id, &[first.id, second.id])
            .await
            .expect("the flags read");
        assert!(flagged.contains(&first.id), "alice bookmarked the first");
        assert!(flagged.contains(&second.id), "and the second");
        assert!(
            !bookmarked_ids(&pool, bob.id, &[first.id, second.id])
                .await
                .expect("bob's flags read")
                .contains(&second.id),
            "but bob only bookmarked the first"
        );
        assert_eq!(
            bookmarked_ids(&pool, alice.id, &[Uuid::now_v7()])
                .await
                .expect("an unbookmarked id reads"),
            HashSet::new(),
            "an id with no row is simply absent"
        );
    }

    #[tokio::test]
    async fn an_empty_id_list_asks_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        assert_eq!(
            bookmarked_ids(&pool, alice.id, &[])
                .await
                .expect("the empty read runs"),
            HashSet::new(),
            "an empty page flags nothing, and the loader skips the query"
        );
    }

    #[tokio::test]
    async fn a_user_page_pairs_each_bookmark_with_its_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = post(&pool, &bob).await;
        let second = post(&pool, &bob).await;
        let third = post(&pool, &bob).await;
        bookmark_as(&pool, &alice, first.id).await;
        bookmark_as(&pool, &alice, second.id).await;
        bookmark_as(&pool, &alice, third.id).await;
        let carol = user(&pool, "carol").await;
        bookmark_as(&pool, &carol, first.id).await;

        let alice_ids = stored_ids(&pool, alice.id).await;

        let pairs = page_for_user(&pool, alice.id, page("limit=2"), None)
            .await
            .expect("the first page reads");
        assert_eq!(
            pair_ids(&pairs),
            vec![alice_ids[0], alice_ids[1]],
            "newest bookmark first, keyed on bookmark ids"
        );
        assert_eq!(
            status_ids(&pairs),
            vec![third.id, second.id],
            "the newest bookmarks name the newest posts"
        );
        assert!(
            pairs
                .iter()
                .all(|(bookmark, status)| bookmark.status_id == status.id),
            "every entry pairs its bookmark with the status it names"
        );

        let older = page_for_user(
            &pool,
            alice.id,
            page(&format!("max_id={}", alice_ids[1])),
            None,
        )
        .await
        .expect("the continuation reads");
        assert_eq!(
            pair_ids(&older),
            vec![alice_ids[2]],
            "max_id walks alice's own bookmark ids"
        );
        assert_eq!(
            older[0].0.user_id, alice.id,
            "carol's bookmark of the same status is on no page of alice's"
        );

        let forward = page_for_user(
            &pool,
            alice.id,
            page(&format!("min_id={}", alice_ids[2])),
            None,
        )
        .await
        .expect("the forward page reads");
        assert_eq!(
            pair_ids(&forward),
            vec![alice_ids[0], alice_ids[1]],
            "min_id answers the newer page, newest-first after the flip"
        );

        assert!(
            page_for_user(&pool, user(&pool, "lonely").await.id, page(""), None)
                .await
                .expect("an account with no bookmarks reads")
                .is_empty(),
            "nobody's list is the empty page"
        );
    }

    #[tokio::test]
    async fn a_folder_narrows_the_page_and_everything_asks_for_all() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = post(&pool, &bob).await;
        let second = post(&pool, &bob).await;
        let third = post(&pool, &bob).await;
        let kept = add(&pool, alice.id, first.id)
            .await
            .expect("the fixture bookmark stores")
            .expect("the pair had nothing to keep");
        bookmark_as(&pool, &alice, second.id).await;
        bookmark_as(&pool, &alice, third.id).await;
        let folder = Uuid::now_v7();
        file_in_folder(&pool, kept.id, folder).await;
        // The fixture filing above set the column the row was read without.
        let mut kept = kept;
        kept.folder_id = Some(folder);

        let all_statuses = vec![third.id, second.id, first.id];
        let filed = page_for_user(&pool, alice.id, page(""), Some(folder))
            .await
            .expect("the folder page reads");
        assert_eq!(
            filed,
            vec![(kept.clone(), first)],
            "the narrow arm answers only the bookmark filed in that folder"
        );

        let everything = page_for_user(&pool, alice.id, page(""), None)
            .await
            .expect("the unfiltered page reads");
        assert_eq!(
            status_ids(&everything),
            all_statuses,
            "None asks for every bookmark, filed or not, newest first"
        );

        assert!(
            page_for_user(&pool, alice.id, page(""), Some(Uuid::now_v7()))
                .await
                .expect("an unknown folder reads")
                .is_empty(),
            "a folder nothing is filed in answers the empty page"
        );
    }

    #[tokio::test]
    async fn a_pair_leaves_the_page_when_its_status_goes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        for _ in 0..PAGE_SEED {
            let bookmarked = post(&pool, &bob).await;
            bookmark_as(&pool, &alice, bookmarked.id).await;
        }
        let full = page_for_user(&pool, alice.id, page(""), None)
            .await
            .expect("the whole list reads");
        assert_eq!(full.len(), PAGE_SEED, "one pair per bookmark");

        // A hard-deleted status drops its whole pair from the page (not
        // the tombstone the endpoint's visibility step handles): a bookmark
        // of a gone post is on no page.
        let gone = full[1].1.id;
        let erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(gone)
            .execute(&pool)
            .await
            .expect("the fixture status deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the status was there to delete");
        let after = page_for_user(&pool, alice.id, page(""), None)
            .await
            .expect("the page after the delete reads");
        assert_eq!(
            status_ids(&after),
            vec![full[0].1.id, full[2].1.id],
            "the gone status contributes nothing and the survivors keep \
             the bookmark-id order"
        );
        assert!(
            after
                .iter()
                .all(|(bookmark, status)| status.id == bookmark.status_id),
            "and every remaining pair still agrees"
        );
    }

    #[tokio::test]
    async fn tombstones_drop_out_of_pages_and_deletes_cascade() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bookmarked = post_by(&pool, "bob").await;
        let kept = post_by(&pool, "carol").await;
        bookmark_as(&pool, &alice, bookmarked).await;
        bookmark_as(&pool, &alice, kept).await;

        // A soft delete leaves the row (and the bookmark) in place: the
        // bookmark survives, and only the page read drops the tombstone —
        // while the visibility gate is the endpoint's step.
        let tombstone = soft_delete(&pool, bookmarked)
            .await
            .expect("the delete runs")
            .expect("the status was live");
        assert!(tombstone.deleted_at.is_some(), "the row is a tombstone");
        let pairs = page_for_user(&pool, alice.id, page(""), None)
            .await
            .expect("the page reads");
        assert_eq!(pairs.len(), 1, "the tombstoned bookmark is off the page");
        assert_eq!(pairs[0].1.id, kept, "and the kept one stays on it");

        // A hard delete takes the bookmark rows with it through the
        // cascade.
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
            vec![bookmarked],
            "only the tombstone's bookmark remains — a hard-deleted status \
             takes its bookmarks with it"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_cascades_their_bookmarks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status_id = post_by(&pool, "bob").await;
        bookmark_as(&pool, &alice, status_id).await;

        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the bookmarked-by account was there to delete");
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

        let err = add(&pool, alice.id, Uuid::now_v7())
            .await
            .expect_err("a status that does not exist cannot be bookmarked");
        assert!(
            matches!(err, AppError::Database(_)),
            "the foreign key refuses, not a silent no-op: {err}"
        );
    }
}
