//! The `announcements` and `announcement_reads` tables: instance-wide notices
//! and who has dismissed which.
//!
//! Reference: Pleroma's `Pleroma.Announcement`
//! (`lib/pleroma/announcement.ex`) and `Pleroma.AnnouncementReadRelationship`
//! (`lib/pleroma/announcement_read_relationship.ex`). Pleroma keeps an
//! announcement's text in two JSONB maps — `data` for what the author typed,
//! `rendered` for the HTML clients see — per `docs/ARCHITECTURE.md` decision 6
//! both become columns here: [`content`](Announcement::content) is the
//! sanitized HTML, [`raw_content`](Announcement::raw_content) the source.
//! [`create`] and [`update`] sanitize whatever they are handed, so the
//! rendered half can never hold markup the sanitizer would have removed.
//!
//! Two facts decide what a user sees. The **activity window**
//! ([`active_for_user`]): a notice is active at an instant when it has no
//! [`starts_at`](Announcement::starts_at) or that moment has passed, and has no
//! [`ends_at`](Announcement::ends_at) or that moment has not arrived; notices
//! outside their window exist but are served to nobody. And the **dismissal**:
//! one row of `announcement_reads` per `(announcement, user)` pair, written by
//! [`mark_read`] and read back as [`read`](ActiveAnnouncement::read), which
//! [`active_for_user`] both reports and — unless asked otherwise — filters on.
//! A dismissal belongs to one account, so hiding a notice never hides it from
//! anybody else.
//!
//! Both list functions answer newest first by comparing the id column
//! (decision 4: `UUIDv7` text sorts in creation order), which is stable within
//! a transaction while the `now()`-stamped `inserted_at` is not.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::{id::new_id, sanitize::sanitize_html},
};

/// [`create`]: the id this module mints (decision 4), the sanitized content and
/// what the caller decided, with `inserted_at` and `updated_at` left to the
/// column defaults. `RETURNING *` brings the row back with those stamps.
const INSERT_SQL: &str = "INSERT INTO announcements (
     id, content, raw_content, starts_at, ends_at, all_day
) VALUES ($1, $2, $3, $4, $5, $6) RETURNING *";

/// [`update`]: every content column of one notice, and the `updated_at` bump
/// Pleroma's `Repo.update` stamps too. No `WHERE`-clause guard on a missing
/// row: [`update`] reports the miss as `None`.
const UPDATE_SQL: &str = "UPDATE announcements
      SET content = $2, raw_content = $3, starts_at = $4, ends_at = $5,
          all_day = $6, updated_at = now()
    WHERE id = $1
  RETURNING *";

/// [`get`]: one notice by id, window and dismissals aside.
const GET_SQL: &str = "SELECT * FROM announcements WHERE id = $1";

/// [`list_all`]: every notice an instance holds, active or not, newest first —
/// the administration view, which sees no viewer and so computes no read flag.
const LIST_ALL_SQL: &str = "SELECT * FROM announcements ORDER BY id DESC";

/// [`delete`]: one notice by id; its dismissals go with it by the foreign key's
/// cascade.
const DELETE_SQL: &str = "DELETE FROM announcements WHERE id = $1";

/// [`active_for_user`]: every notice active at `$1` (the instant the call is
/// made at), each with whether user `$2` dismissed it. The text stops at the
/// window condition, where the caller pushes what it decides on:
/// [`DISMISSED_OUT_SQL`], when the default list is asked for, and then
/// [`NEWEST_FIRST_SQL`].
///
/// The window condition is Pleroma's `list_all_visible_when`: a `NULL` bound is
/// an open end, so a notice with neither has always been active and one with
/// only `starts_at` never stops being active. `$1` is compared inclusively at
/// both ends (Mastodon's `starts_at <= now`, `ends_at >= now`).
const ACTIVE_SQL: &str = "SELECT a.*, EXISTS (
           SELECT 1 FROM announcement_reads r
            WHERE r.announcement_id = a.id AND r.user_id = $2
       ) AS read
      FROM announcements a
     WHERE (a.starts_at IS NULL OR a.starts_at <= $1)
       AND (a.ends_at IS NULL OR a.ends_at >= $1)";

/// What [`active_for_user`] pushes when `with_dismissed` is `false`: the notices
/// this user dismissed leave their list, while every other account still sees
/// them.
const DISMISSED_OUT_SQL: &str = " AND NOT EXISTS (
           SELECT 1 FROM announcement_reads r
            WHERE r.announcement_id = a.id AND r.user_id = $2)";

/// What [`active_for_user`] pushes last: newest first, on the id whose text
/// sorts in creation order (decision 4).
const NEWEST_FIRST_SQL: &str = " ORDER BY a.id DESC";

/// [`mark_read`]: one dismissal, written once. `ON CONFLICT DO NOTHING` is what
/// makes dismissing twice the no-op it should be — Pleroma's plain insert
/// instead hits its unique constraint — and the statement reports through
/// `rows_affected` whether a marker went in.
const MARK_READ_SQL: &str = "INSERT INTO announcement_reads (announcement_id, user_id)
     VALUES ($1, $2)
ON CONFLICT (announcement_id, user_id) DO NOTHING";

/// One row of `announcements`: an instance-wide notice.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Announcement {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); also the
    /// creation order lists are read in.
    pub id: Uuid,
    /// The notice as sanitized HTML — what clients render. [`create`] and
    /// [`update`] run the sanitizer, so this column cannot hold more than it
    /// allows.
    pub content: String,
    /// The notice as its author wrote it, kept verbatim for the
    /// `pleroma.raw_content` key of the entity.
    pub raw_content: String,
    /// When the notice became active; `None` means it was active before any
    /// time one could name.
    pub starts_at: Option<DateTime<Utc>>,
    /// When it stops being active; `None` means it never does.
    pub ends_at: Option<DateTime<Utc>>,
    /// Whether clients show the window's dates without their times.
    pub all_day: bool,
    /// When the notice was created; the entity's `published_at`.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every [`update`].
    pub updated_at: DateTime<Utc>,
}

/// An [`Announcement`] as one account sees it: the row plus that viewer's
/// dismissal, the shape [`active_for_user`] answers with.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ActiveAnnouncement {
    /// The notice itself.
    #[sqlx(flatten)]
    pub announcement: Announcement,
    /// Whether the user [`active_for_user`] was called for has dismissed it.
    pub read: bool,
}

/// What to create, or to replace a notice's content with: every
/// `announcements` column except the `id` [`create`] mints and the timestamps
/// the database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets only what
/// it decides on; the usual minimum is the text.
#[derive(Clone, Debug)]
pub struct NewAnnouncement {
    /// The HTML to serve: [`create`] and [`update`] sanitize it into
    /// [`Announcement::content`], so anything the author could not have written
    /// is gone before the row is written. Empty in the default.
    pub content: String,
    /// The same text as its author wrote it, stored verbatim in
    /// [`Announcement::raw_content`]; empty in the default.
    pub raw_content: String,
    /// Start of the activity window; `None` by default, like the column.
    pub starts_at: Option<DateTime<Utc>>,
    /// End of the activity window; `None` by default, like the column.
    pub ends_at: Option<DateTime<Utc>>,
    /// Date-only display; `false` by default, like the column.
    pub all_day: bool,
}

/// One notice authored from a single piece of text, which is both its source
/// and — once sanitized — its content: what Pleroma's `changeset` does with
/// `data["content"]` and the `rendered` map it derives.
impl From<&str> for NewAnnouncement {
    fn from(text: &str) -> Self {
        Self {
            content: text.to_owned(),
            raw_content: text.to_owned(),
            starts_at: None,
            ends_at: None,
            all_day: false,
        }
    }
}

impl Default for NewAnnouncement {
    /// The column defaults: empty text in both spellings, an open window and
    /// timestamps shown.
    fn default() -> Self {
        Self {
            content: String::new(),
            raw_content: String::new(),
            starts_at: None,
            ends_at: None,
            all_day: false,
        }
    }
}

/// Inserts `notice`, sanitizing its content, and returns the stored row.
///
/// The primary key is minted here (decision 4), so the row's place in the
/// newest-first lists is its creation time.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, notice: &NewAnnouncement) -> AppResult<Announcement> {
    let content = sanitize_html(&notice.content);
    let row = sqlx::query_as::<_, Announcement>(INSERT_SQL)
        .bind(new_id())
        .bind(&content)
        .bind(&notice.raw_content)
        .bind(notice.starts_at)
        .bind(notice.ends_at)
        .bind(notice.all_day)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Replaces every content column of the notice `id` with `notice` — sanitizing
/// its content, as [`create`] does — and returns the rewritten row, `None` when
/// no notice carries that id.
///
/// This is a whole-row write, the way Pleroma's admin form submits a notice: a
/// caller that changes one field reads the row with [`get`], adjusts the field
/// and sends the rest back unchanged.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    notice: &NewAnnouncement,
) -> AppResult<Option<Announcement>> {
    let content = sanitize_html(&notice.content);
    let row = sqlx::query_as::<_, Announcement>(UPDATE_SQL)
        .bind(id)
        .bind(&content)
        .bind(&notice.raw_content)
        .bind(notice.starts_at)
        .bind(notice.ends_at)
        .bind(notice.all_day)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Fetches the notice `id`, whatever its window says: [`get`] answers a notice
/// outside its window too, because that is what an administration view needs.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Option<Announcement>> {
    let row = sqlx::query_as::<_, Announcement>(GET_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Fetches every notice the instance holds, newest first, active or not, with
/// no viewer's dismissal state (see [`list_all`]'s counterpart
/// [`active_for_user`] for that).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_all(pool: &PgPool) -> AppResult<Vec<Announcement>> {
    let rows = sqlx::query_as::<_, Announcement>(LIST_ALL_SQL)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Deletes the notice `id`, reporting whether one went. Its dismissals are
/// deleted with it, so an id reused by [`create`] arrives unread.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_SQL).bind(id).execute(pool).await?;
    Ok(deleted.rows_affected() > 0)
}

/// The notices active *now* for `user_id`, newest first: those whose window
/// holds the instant the call is made at (see the `ACTIVE_SQL` constant),
/// carrying whether `user_id` dismissed it.
///
/// Dismissed notices are left out unless `with_dismissed` is set, which is the
/// difference between the list a client shows and the list a client that asks
/// for everything shows — either way the [`read`](ActiveAnnouncement::read)
/// flag describes the same thing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn active_for_user(
    pool: &PgPool,
    user_id: Uuid,
    with_dismissed: bool,
) -> AppResult<Vec<ActiveAnnouncement>> {
    let now = Utc::now();
    let mut builder = QueryBuilder::<Postgres>::new(ACTIVE_SQL);
    if !with_dismissed {
        let _filter: &mut _ = builder.push(DISMISSED_OUT_SQL);
    }
    let _order: &mut _ = builder.push(NEWEST_FIRST_SQL);
    let rows = builder
        .build_query_as::<ActiveAnnouncement>()
        .bind(now)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Records that `user_id` has dismissed the notice `announcement_id`, `true`
/// when this call wrote the marker and `false` when it was already there.
///
/// The announcement's id is a foreign key, so a caller that has not confirmed
/// the notice exists (with [`get`], as the dismiss endpoint does) fails on the
/// constraint rather than reporting a dismissal of nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding, the missing notice) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_read(pool: &PgPool, announcement_id: Uuid, user_id: Uuid) -> AppResult<bool> {
    let written = sqlx::query(MARK_READ_SQL)
        .bind(announcement_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(written.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, User, insert, local_ap_id},
        util::time::parse_time,
    };

    /// An instant from its ISO 8601 text, for a window bound that has to
    /// survive the database's microsecond resolution unchanged.
    fn at(text: &str) -> DateTime<Utc> {
        parse_time(text).expect("the fixture times are timestamps")
    }

    /// An instant `days` from now, negative ones for the past. Only ever used
    /// to decide which side of *now* a window bound falls on, never compared
    /// back to what the row stores.
    fn offset(days: i64) -> DateTime<Utc> {
        Utc::now() + Duration::days(days)
    }

    /// A notice with `content` as both its text spellings and an open window.
    fn text(content: &str) -> NewAnnouncement {
        NewAnnouncement::from(content)
    }

    /// Inserts a notice named `content` with the window `starts_at`/`ends_at`,
    /// and returns its row, for a test that goes on to dismiss or delete it.
    async fn notice(
        pool: &PgPool,
        content: &str,
        starts_at: Option<DateTime<Utc>>,
        ends_at: Option<DateTime<Utc>>,
    ) -> Announcement {
        create(
            pool,
            &NewAnnouncement {
                starts_at,
                ends_at,
                ..text(content)
            },
        )
        .await
        .expect("fixture notice creates")
    }

    /// [`notice`] with its row dropped: a test that only lists what it seeded.
    async fn seed(
        pool: &PgPool,
        content: &str,
        starts_at: Option<DateTime<Utc>>,
        ends_at: Option<DateTime<Utc>>,
    ) {
        let _row: Announcement = notice(pool, content, starts_at, ends_at).await;
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// The `(body, read)` pairs of `rows`, in row order — what the dismissal
    /// tests compare against, list order and read flags in one glance.
    fn flags(rows: &[ActiveAnnouncement]) -> Vec<(&str, bool)> {
        rows.iter()
            .map(|row| (row.announcement.content.as_str(), row.read))
            .collect()
    }

    /// The bodies of `rows`, in row order — what a list test compares against.
    fn bodies(rows: &[ActiveAnnouncement]) -> Vec<&str> {
        rows.iter()
            .map(|row| row.announcement.content.as_str())
            .collect()
    }

    #[tokio::test]
    async fn create_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let starts = at("2000-06-01T12:00:00.250Z");
        let ends = at("2100-06-01T12:00:00.250Z");
        let stored = create(
            &pool,
            &NewAnnouncement {
                starts_at: Some(starts),
                ends_at: Some(ends),
                all_day: true,
                content: "<p>hello</p><script>steal()</script>".to_owned(),
                raw_content: "hello <script>steal()</script>".to_owned(),
            },
        )
        .await
        .expect("a notice creates");

        assert!(stored.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(
            stored.content, "<p>hello</p>",
            "the served content is sanitized"
        );
        assert_eq!(
            stored.raw_content, "hello <script>steal()</script>",
            "the source is kept verbatim"
        );
        assert_eq!(stored.starts_at, Some(starts), "the window round trips");
        assert_eq!(stored.ends_at, Some(ends), "both ends of it");
        assert!(stored.all_day, "the date-only flag round trips");
        assert_eq!(
            stored.inserted_at, stored.updated_at,
            "a fresh row is stamped once"
        );
    }

    #[tokio::test]
    async fn create_defaults_match_the_column_defaults() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stored = create(&pool, &text("hi"))
            .await
            .expect("a bare notice creates");
        assert_eq!(stored.content, "hi", "plain text is its own content");
        assert_eq!(stored.raw_content, "hi", "and its own source");
        assert!(stored.starts_at.is_none(), "the window opens at once");
        assert!(stored.ends_at.is_none(), "and never closes");
        assert!(!stored.all_day, "times are shown by default");

        let empty = NewAnnouncement::default();
        assert!(
            empty.content.is_empty() && empty.raw_content.is_empty() && !empty.all_day,
            "the default text is empty, not missing, and the window open"
        );
    }

    #[tokio::test]
    async fn active_for_user_serves_only_notices_in_their_window() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        // Active: an open window, one open at either end, one spanning now.
        seed(&pool, "open", None, None).await;
        seed(&pool, "started", Some(offset(-2)), None).await;
        seed(&pool, "ending", None, Some(offset(2))).await;
        seed(&pool, "spanning", Some(offset(-2)), Some(offset(2))).await;
        // Inactive: the four ways a window can exclude the present, each half
        // on its own and both together.
        seed(&pool, "later", Some(offset(2)), None).await;
        seed(&pool, "over", None, Some(offset(-2))).await;
        seed(&pool, "done", Some(offset(-3)), Some(offset(-2))).await;
        seed(&pool, "unstarted", Some(offset(2)), Some(offset(3))).await;

        let listed = active_for_user(&pool, alice.id, false)
            .await
            .expect("the active list reads");
        let mut found = bodies(&listed);
        found.sort_unstable();
        assert_eq!(
            found,
            ["ending", "open", "spanning", "started"],
            "everything whose window holds this instant, and only that"
        );
    }

    #[tokio::test]
    async fn active_for_user_answers_newest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        seed(&pool, "first", None, None).await;
        seed(&pool, "second", None, None).await;
        seed(&pool, "third", Some(offset(-1)), Some(offset(1))).await;

        let listed = active_for_user(&pool, alice.id, false)
            .await
            .expect("the active list reads");
        assert_eq!(
            bodies(&listed),
            ["third", "second", "first"],
            "creation order reversed: ids sort by time, `inserted_at` does not"
        );
    }

    #[tokio::test]
    async fn active_for_user_flags_and_filters_dismissals() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = notice(&pool, "first", None, None).await;
        seed(&pool, "second", None, None).await;
        let marked = mark_read(&pool, first.id, alice.id)
            .await
            .expect("the dismissal writes");
        assert!(marked, "the first dismissal writes a marker");

        let kept = active_for_user(&pool, alice.id, true)
            .await
            .expect("the dismissed-inclusive list reads");
        assert_eq!(
            flags(&kept),
            [("second", false), ("first", true)],
            "with `with_dismissed` the dismissed notice is listed, flagged read"
        );

        let hidden = active_for_user(&pool, alice.id, false)
            .await
            .expect("the default list reads");
        assert_eq!(
            flags(&hidden),
            [("second", false)],
            "without it, it leaves the list"
        );

        let for_bob = active_for_user(&pool, bob.id, false)
            .await
            .expect("the other viewer's list reads");
        assert_eq!(
            flags(&for_bob),
            [("second", false), ("first", false)],
            "alice's dismissal hides nothing from bob and reads nothing to him"
        );
    }

    #[tokio::test]
    async fn mark_read_is_idempotent_once_per_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let news = notice(&pool, "news", None, None).await;

        assert!(
            mark_read(&pool, news.id, alice.id)
                .await
                .expect("the first dismissal writes"),
            "the first call writes alice's marker"
        );
        assert!(
            !(mark_read(&pool, news.id, alice.id)
                .await
                .expect("a repeat dismissal does not fail")),
            "the second finds the key taken and writes nothing"
        );
        assert!(
            mark_read(&pool, news.id, bob.id)
                .await
                .expect("bob's dismissal writes"),
            "another account's marker is its own row"
        );

        let listed = active_for_user(&pool, alice.id, true)
            .await
            .expect("the list reads");
        assert_eq!(
            flags(&listed),
            [("news", true)],
            "alice still sees her one notice, flagged read"
        );
    }

    #[tokio::test]
    async fn active_for_user_of_nothing_is_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        assert!(
            active_for_user(&pool, alice.id, true)
                .await
                .expect("an empty list reads")
                .is_empty(),
            "no rows, no notices"
        );

        // A viewer with no marker rows — even one this instance has never
        // seen — still gets every active notice, none of them read.
        seed(&pool, "news", None, None).await;
        let listed = active_for_user(&pool, Uuid::now_v7(), false)
            .await
            .expect("the stranger's list reads");
        assert_eq!(flags(&listed), [("news", false)], "the list is served");
    }

    #[tokio::test]
    async fn update_rewrites_every_column_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stored = notice(&pool, "before", Some(offset(-1)), Some(offset(1))).await;
        let rewritten = update(
            &pool,
            stored.id,
            &NewAnnouncement {
                starts_at: None,
                ends_at: None,
                all_day: true,
                content: "<b>after</b><em> too</em>".to_owned(),
                raw_content: "after too".to_owned(),
            },
        )
        .await
        .expect("a notice updates")
        .expect("the fixture notice exists");

        assert_eq!(rewritten.id, stored.id, "the row is rewritten in place");
        assert_eq!(
            rewritten.content, "<b>after</b><em> too</em>",
            "allowed markup survives the sanitizer"
        );
        assert_eq!(
            rewritten.raw_content, "after too",
            "the source is replaced too"
        );
        assert!(
            rewritten.starts_at.is_none() && rewritten.ends_at.is_none(),
            "an open window replaces a closed one"
        );
        assert!(rewritten.all_day, "the flag is replaced");
        assert_eq!(
            rewritten.inserted_at, stored.inserted_at,
            "the creation stamp stays"
        );
        assert!(
            rewritten.updated_at >= stored.updated_at,
            "the update stamp moves forward"
        );
        let fetched = get(&pool, stored.id)
            .await
            .expect("the notice reads back")
            .expect("it is still there");
        assert_eq!(fetched, rewritten, "and reads back as written");
    }

    #[tokio::test]
    async fn update_and_get_of_an_unknown_id_find_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let missing = Uuid::now_v7();
        assert!(
            update(&pool, missing, &text("ghost"))
                .await
                .expect("updating nothing is not a failure")
                .is_none(),
            "no row, no rewrite"
        );
        assert!(
            get(&pool, missing)
                .await
                .expect("reading nothing is not a failure")
                .is_none(),
            "and no row to read back"
        );
    }

    #[tokio::test]
    async fn get_serves_a_notice_outside_its_window() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let expired = notice(&pool, "expired", Some(offset(-3)), Some(offset(-2))).await;
        let fetched = get(&pool, expired.id)
            .await
            .expect("the notice reads")
            .expect("an inactive notice is still a row");
        assert_eq!(fetched, expired, "the whole row, window aside");
        assert!(
            active_for_user(&pool, Uuid::now_v7(), true)
                .await
                .expect("the list reads")
                .is_empty(),
            "while the active list hides it"
        );
    }

    #[tokio::test]
    async fn list_all_is_newest_first_and_sees_every_notice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        seed(&pool, "old", None, None).await;
        seed(&pool, "upcoming", Some(offset(2)), None).await;
        seed(&pool, "new", None, None).await;

        let listed = list_all(&pool).await.expect("the admin list reads");
        assert_eq!(
            listed
                .iter()
                .map(|row| row.content.as_str())
                .collect::<Vec<_>>(),
            ["new", "upcoming", "old"],
            "every notice, active or not, newest first"
        );
        let upcoming = listed
            .iter()
            .find(|row| row.content == "upcoming")
            .expect("the upcoming notice is listed");
        assert!(
            delete(&pool, upcoming.id)
                .await
                .expect("the notice deletes"),
            "and deletes like any other"
        );
        assert_eq!(
            list_all(&pool)
                .await
                .expect("the admin list reads again")
                .len(),
            2,
            "afterwards one row less"
        );
    }

    #[tokio::test]
    async fn delete_takes_a_notice_once_and_with_it_its_reads() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let news = notice(&pool, "news", None, None).await;
        let marked = mark_read(&pool, news.id, alice.id)
            .await
            .expect("the dismissal writes");
        assert!(marked, "the dismissal writes a marker");

        assert!(
            delete(&pool, news.id).await.expect("the notice deletes"),
            "the first delete finds it"
        );
        assert!(
            !(delete(&pool, news.id)
                .await
                .expect("deleting nothing is not a failure")),
            "the second finds nothing"
        );
        assert!(
            get(&pool, news.id).await.expect("the row reads").is_none(),
            "and it is gone"
        );
        assert_eq!(marker_count(&pool).await, 0, "its dismissal went too");
    }

    #[tokio::test]
    async fn deleting_a_user_forgets_what_they_dismissed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let news = notice(&pool, "news", None, None).await;
        let marked = mark_read(&pool, news.id, alice.id)
            .await
            .expect("the dismissal writes");
        assert!(marked, "the dismissal writes a marker");
        assert_eq!(marker_count(&pool).await, 1, "the marker is there to go");

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(
            marker_count(&pool).await,
            0,
            "and their dismissals with it, while the notice stays"
        );
        assert!(
            get(&pool, news.id)
                .await
                .expect("the notice reads")
                .is_some(),
            "deleting an account leaves the notice itself alone"
        );
    }

    /// How many dismissal rows the database holds.
    async fn marker_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM announcement_reads")
            .fetch_one(pool)
            .await
            .expect("the read markers count")
    }

    #[tokio::test]
    async fn create_sanitizes_the_served_content_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let source = r#"<p>See <a href="javascript:alert(1)">this</a></p>"#;
        let stored = create(&pool, &text(source))
            .await
            .expect("a notice with a link creates");
        assert_eq!(
            stored.content, r#"<p>See <a rel="ugc nofollow noopener noreferrer">this</a></p>"#,
            "the dangerous href is gone, the text and the tag are not"
        );
        assert_eq!(
            stored.raw_content, source,
            "while the source keeps every byte the author wrote"
        );
    }
}
