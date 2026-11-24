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
//! header walk, not the joined row's. A fourth read, [`backup_page`],
//! walks the same id the other way for the export job in
//! [`crate::jobs::backups`].
//!
//! Deleting a user or a status takes its favourites through the foreign
//! key's cascade. The [`Favourite::ap_id`] awaits the federation ticket
//! that sends the `Like` activity (decision 6); [`favourite`] mints a
//! local one.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        status::{self, Counter, Status},
        user::User,
    },
    pagination::{PageParams, keyset_page_by},
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
/// is where [`keyset_page_by`] pushes its bind and the builder is what numbers
/// it `$1`; the keyset it then appends runs on the favourite id `f.id`.
/// Deactivated accounts (`u.is_active` false) are left out, the way
/// Mastodon's `without_suspended` scope drops them.
const USERS_PAGE_SQL: &str = "SELECT f.id AS favourite_id, u.* FROM favourites f
    JOIN users u ON u.id = f.user_id
     WHERE u.is_active AND f.status_id = ";

/// [`page_for_user`]: one account's favourites, the full [`Favourite`]
/// row leading — the id the page keys on, which the endpoint's `Link`
/// header carries rather than the status id. The joined statuses come from
/// [`status::get_many`] in a second read, so the keyset stays a plain walk
/// of the `(user_id, id desc)` pair the table's migration indexes.
const FAVOURITES_PAGE_SQL: &str = "SELECT f.* FROM favourites f
     WHERE f.user_id = ";

/// [`backup_page`]: one account's favourites walked in favourite id order,
/// each row joined to its status so the archive item is the liked
/// object's `uri` rather than an id the caller must look up again. Like
/// [`FAVOURITES_PAGE_SQL`] the text stops where its one placeholder
/// belongs, because that is where [`backup_page`] pushes the author's id
/// bind; the function then appends the `after` cursor, the ascending
/// ordering and the `LIMIT` on `f.id`.
const BACKUP_PAGE_SQL: &str = "SELECT f.id, s.uri FROM favourites f
    JOIN statuses s ON s.id = f.status_id
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
    favourite_like(pool, user_id, status_id, &local_activity_id(base_url)).await
}

/// Records `user_id`'s like of `status_id` standing for one `Like` activity
/// with the explicit `ap_id`.
///
/// Idempotent on `(user_id, status_id)` exactly like [`favourite`]: a pair
/// that already liked keeps its row — id, `ap_id` and all — steps nothing
/// and answers `None`. This is the federation mirror's write (decision 6 —
/// a remote activity's `ap_id` is whatever the remote sent, so the
/// activity's own `Undo` names it back and [`get_by_ap_id`] finds the row
/// by it); [`favourite`] is this same statement under a freshly minted
/// local id.
///
/// [`get_by_ap_id`]: get_by_ap_id
///
/// # Errors
///
/// Those of [`favourite`].
pub async fn favourite_like(
    pool: &PgPool,
    user_id: Uuid,
    status_id: Uuid,
    ap_id: &str,
) -> AppResult<Option<Favourite>> {
    let created = sqlx::query_as::<_, Favourite>(FAVOURITE_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(status_id)
        .bind(ap_id)
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
    keyset_page_by(pool, USERS_PAGE_SQL, status_id, "f.id", &page).await
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
    let favourites: Vec<Favourite> =
        keyset_page_by(pool, FAVOURITES_PAGE_SQL, user_id, "f.id", &page).await?;
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

/// One ascending page of `user_id`'s favourites for the backup archive
/// ([`crate::jobs::backups`]).
///
/// Each entry is the favourite row's id — the keyset cursor the export
/// walks, exactly as [`page_for_user`] keys its pages on the like id — and
/// the liked status's `uri`, the item the archive lists. A favourite whose
/// status row is gone entirely (hard-deleted, cascaded) contributes
/// nothing; a soft-deleted one still rides along, its reserved `uri` part
/// of the account's history. `after`, when given, admits only rows with a
/// strictly greater favourite id, at `limit` at a time, so the whole walk
/// costs pages of at most `limit` rows.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn backup_page(
    pool: &PgPool,
    user_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> AppResult<Vec<(Uuid, String)>> {
    let mut builder = QueryBuilder::<Postgres>::new(BACKUP_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    if let Some(after) = after {
        let _newer: &mut _ = builder.push(" AND f.id > ").push_bind(after);
    }
    let _paged: &mut _ = builder.push(" ORDER BY f.id ASC LIMIT ").push_bind(limit);
    let rows = builder
        .build_query_as::<(Uuid, String)>()
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The `ap_id` [`favourite`] stores: a freshly minted
/// `{base_url}/activities/{uuid}` (decision 6). Every favourite this model
/// writes comes from a local account through the API; the federation
/// ticket that mirrors a remote `Like` records that activity's own id.
fn local_activity_id(base_url: &str) -> String {
    format!("{base_url}/activities/{}", new_id())
}

#[cfg(test)]
mod tests;
