//! The `markers` table: where one account left off reading each timeline.
//!
//! Reference: Pleroma's `Pleroma.Marker` (`lib/pleroma/marker.ex`). Pleroma
//! identifies a marker by the `(user_id, timeline)` pair its unique index
//! keeps, and writes one with `on_conflict: {:replace, [:last_read_id]}`; the
//! same conflict target is `UPSERT_SUFFIX` here. Its `lock_version` column is
//! this table's [`version`](Marker::version), which the statement itself
//! increments: a first write leaves the column default of `0`, every later one
//! adds one, so a client can tell how often its position moved.
//!
//! A marker names the newest entry its account has read by id *text* and
//! nothing else: [`last_read_id`](Marker::last_read_id) is stored verbatim
//! up to `MAX_LAST_READ_ID_CHARS` characters — a longer one is cut, not
//! refused — and only the `notifications` timeline ever reads through it,
//! as [`unread_notifications_count`] compares it against
//! [`notifications`](crate::models::notification) ids. It is never joined
//! to [`statuses`](crate::models::status).
//!
//! Markers are strictly per account: [`get_many`] and [`upsert_many`] scope
//! every read and write to `user_id`, so two accounts marking the same timeline
//! are two independent rows and one account never reads or moves another's.
//! Deleting a user takes their markers with it through the foreign key's
//! cascade.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::id::{new_id, parse_id},
};

/// The `home` timeline: the account's front page, the marker whose
/// `pleroma.unread_count` is always `0` (see the API's
/// [`markers`](crate::api::markers)).
pub const HOME: &str = "home";
/// The `notifications` timeline: the marker that carries an unread count,
/// which [`unread_notifications_count`] computes.
pub const NOTIFICATIONS: &str = "notifications";

/// Longest `last_read_id` [`upsert_many`] stores: a longer one is cut to this
/// many characters by [`capped`].
const MAX_LAST_READ_ID_CHARS: usize = 255;

/// [`get_many`]: the markers of one account among the timelines asked for.
/// `$2` is cast to `text[]` so an empty request binds as the empty array
/// rather than as an untypeable `NULL`; ordering by the timeline name keeps the
/// answer the same whatever the planner picks.
const GET_MANY_SQL: &str = "SELECT * FROM markers
     WHERE user_id = $1 AND timeline = ANY($2::text[])
  ORDER BY timeline";

/// [`upsert_many`], before the `VALUES` list it generates: the id this module
/// mints (decision 4) and the pair's own two columns. A write that conflicts
/// keeps the row's stored id, so a marker is one row per account and timeline,
/// rewritten in place; `version` and `updated_at` are the conflict clause's and
/// the column defaults' business.
const UPSERT_PREFIX: &str = "INSERT INTO markers (id, user_id, timeline, last_read_id) ";

/// [`upsert_many`], after that list: Pleroma's replace-the-position upsert,
/// with the `version` bump Ecto's `lock_version` performs and an
/// `updated_at` restamp. `RETURNING *` brings every written row back with the
/// version and timestamp the database just gave it.
const UPSERT_SUFFIX: &str = "
ON CONFLICT (user_id, timeline) DO UPDATE SET
      last_read_id = EXCLUDED.last_read_id,
      version = markers.version + 1,
      updated_at = now()
  RETURNING *";

/// One row of `markers`: one account's read position in one timeline.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Marker {
    /// `UUIDv7` primary key, minted by the insert [`upsert_many`] runs
    /// (decision 4); a later upsert of the same `(user_id, timeline)` keeps it.
    pub id: Uuid,
    /// The owning account; every read and write here is scoped to it.
    pub user_id: Uuid,
    /// Which timeline the position is for: [`HOME`] or [`NOTIFICATIONS`].
    pub timeline: String,
    /// Id text of the newest entry the account has read, verbatim up to
    /// `MAX_LAST_READ_ID_CHARS` characters; empty when nothing is read yet.
    pub last_read_id: String,
    /// How many times the row has been written: `0` for the first marker, one
    /// more for every [`upsert_many`] that moved it.
    pub version: i32,
    /// When the position last changed; [`upsert_many`] restamps it on each
    /// write, so it is the marker's own `updated_at`, not the row's creation.
    pub updated_at: DateTime<Utc>,
}

/// The markers of `user_id` for the timelines in `timelines`, ordered by
/// timeline name.
///
/// Timelines the account has no marker for are simply absent: a missing marker
/// is not an empty one, and the API reports only the timelines that have a row.
/// An empty `timelines` reads nothing, and so does one naming only unknown
/// timelines.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(pool: &PgPool, user_id: Uuid, timelines: &[&str]) -> AppResult<Vec<Marker>> {
    let timelines = timelines.to_vec();
    let rows = sqlx::query_as::<_, Marker>(GET_MANY_SQL)
        .bind(user_id)
        .bind(&timelines)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Writes one marker per pair of `markers`, creating the account's row for a
/// timeline it has none for and moving the existing one otherwise, and returns
/// every row it touched, ordered by timeline name.
///
/// A created row starts at version `0`; updating one adds `1` to its `version`
/// and restamps `updated_at`. Each `last_read_id` is stored through `capped`.
/// Pairs naming the same timeline twice collapse to their **last** value —
/// PostgreSQL rejects a write that would touch one conflict key twice, and the
/// last word is what a client that sent the key twice means. An empty
/// `markers` writes nothing and answers an empty list.
///
/// A `timeline` outside [`HOME`]/[`NOTIFICATIONS`] violates the table's check
/// constraint and fails as a database error: the API validates first, so this
/// is a programming error, not a client one.
///
/// # Errors
///
/// Any database failure (connection, encoding, the constraint above) arrives
/// as [`AppError::Database`](crate::error::AppError).
pub async fn upsert_many(
    pool: &PgPool,
    user_id: Uuid,
    markers: Vec<(&str, &str)>,
) -> AppResult<Vec<Marker>> {
    let values = marker_values(markers);
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Postgres>::new(UPSERT_PREFIX);
    let _statement: &mut _ = builder
        .push_values(
            values.iter().map(|(name, id)| (*name, id.as_str())),
            |mut row, (timeline, last_read_id)| {
                let _columns: &mut _ = row
                    .push_bind(new_id())
                    .push_bind(user_id)
                    .push_bind(timeline)
                    .push_bind(last_read_id);
            },
        )
        .push(UPSERT_SUFFIX);
    let mut rows = builder.build_query_as::<Marker>().fetch_all(pool).await?;
    rows.sort_by(|a, b| a.timeline.cmp(&b.timeline));
    Ok(rows)
}

/// How many notifications of `user_id` are still unseen past `last_read_id`,
/// the `pleroma.unread_count` a [`NOTIFICATIONS`] marker reports.
///
/// Pleroma computes it in `Marker.unread_count_query`
/// (`lib/pleroma/marker.ex`): the account's undismissed, unseen
/// notifications created after the marker's position — the same predicate
/// `GET /api/v2/notifications/unread_count` answers with, without that
/// endpoint's type filters and cap (see
/// [`notification::unread_count`](crate::models::notification::unread_count)).
/// A position that names no id — the empty default, or text that is not
/// id — counts from the beginning of time, Pleroma's `restrict_after_marker`
/// skipping its clause the same way.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unread_notifications_count(
    pool: &PgPool,
    user_id: Uuid,
    last_read_id: &str,
) -> AppResult<i64> {
    let past = parse_id(last_read_id);
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM notifications
          WHERE user_id = $1 AND NOT seen AND dismissed_at IS NULL
            AND ($2::uuid IS NULL OR id > $2)",
    )
    .bind(user_id)
    .bind(past)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// The pairs [`upsert_many`] writes, in the order they were given: each
/// timeline once (later duplicates overwrite earlier ones, see
/// [`upsert_many`]) and each position through [`capped`].
fn marker_values<'a>(markers: Vec<(&'a str, &'a str)>) -> Vec<(&'a str, String)> {
    let mut values: Vec<(&str, String)> = Vec::with_capacity(markers.len());
    for (timeline, last_read_id) in markers {
        if let Some(existing) = values.iter_mut().find(|existing| existing.0 == timeline) {
            existing.1 = capped(last_read_id);
        } else {
            values.push((timeline, capped(last_read_id)));
        }
    }
    values
}

/// `text` up to [`MAX_LAST_READ_ID_CHARS`] characters, kept verbatim below
/// that: the cut counts characters, not bytes, so multi-byte id text is never
/// split mid-character.
fn capped(text: &str) -> String {
    text.chars().take(MAX_LAST_READ_ID_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{models::user::User, test_fixtures::local_user as user};

    /// The timelines of `rows`, in order — what a read test compares against.
    fn timelines(rows: &[Marker]) -> Vec<&str> {
        rows.iter().map(|row| row.timeline.as_str()).collect()
    }

    /// Marks `timeline` at `last_read_id` for `user`, dropping the rows.
    async fn mark(pool: &PgPool, owner: &User, timeline: &str, last_read_id: &str) {
        let _rows: Vec<Marker> = upsert_many(pool, owner.id, vec![(timeline, last_read_id)])
            .await
            .expect("the fixture marker upserts");
    }

    #[tokio::test]
    async fn a_first_marker_is_written_at_version_zero() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let written = upsert_many(&pool, alice.id, vec![(HOME, "0193abcd")])
            .await
            .expect("a marker upserts");

        assert_eq!(written.len(), 1, "one pair, one row");
        let marker = written.first().expect("the written row is in the answer");
        assert!(marker.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(marker.user_id, alice.id, "the owner is the argument");
        assert_eq!(marker.timeline, HOME, "the timeline is the pair's");
        assert_eq!(marker.last_read_id, "0193abcd", "the position verbatim");
        assert_eq!(marker.version, 0, "a first marker is version 0");
        assert!(
            marker.updated_at <= Utc::now(),
            "and stamped with the write's instant"
        );
    }

    #[tokio::test]
    async fn rewriting_a_marker_bumps_version_and_keeps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let first = upsert_many(&pool, alice.id, vec![(HOME, "one")])
            .await
            .expect("the first write lands")
            .into_iter()
            .next()
            .expect("one row comes back");
        let second = upsert_many(&pool, alice.id, vec![(HOME, "two")])
            .await
            .expect("the second write lands")
            .into_iter()
            .next()
            .expect("one row comes back");

        assert_eq!(second.id, first.id, "the same row, rewritten in place");
        assert_eq!(second.last_read_id, "two", "at its new position");
        assert_eq!(second.version, first.version + 1, "one version up");
        assert!(second.updated_at >= first.updated_at, "and restamped");

        let third = upsert_many(&pool, alice.id, vec![(HOME, "three")])
            .await
            .expect("a third write lands");
        assert_eq!(
            third.first().expect("the row").version,
            2,
            "the counter keeps counting"
        );
        let stored = get_many(&pool, alice.id, &[HOME])
            .await
            .expect("the marker reads back");
        assert_eq!(stored.len(), 1, "and there is still only one of them");
    }

    #[tokio::test]
    async fn both_timelines_are_written_by_one_call() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let written = upsert_many(&pool, alice.id, vec![(NOTIFICATIONS, "n-1"), (HOME, "h-1")])
            .await
            .expect("a two-timeline write lands");

        assert_eq!(
            timelines(&written),
            [HOME, NOTIFICATIONS],
            "both rows come back, ordered by timeline"
        );
        assert!(
            written.iter().all(|row| row.version == 0),
            "each is a first marker"
        );
    }

    #[tokio::test]
    async fn a_repeated_timeline_is_written_once_from_its_last_value() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let written = upsert_many(&pool, alice.id, vec![(HOME, "first"), (HOME, "second")])
            .await
            .expect("duplicated pairs are not an error");

        assert_eq!(written.len(), 1, "one row for the doubled timeline");
        assert_eq!(
            written.first().expect("the row").last_read_id,
            "second",
            "the last word wins"
        );
        assert_eq!(
            written.first().expect("the row").version,
            0,
            "which is still the marker's first write"
        );
    }

    #[tokio::test]
    async fn nothing_to_write_answers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let written = upsert_many(&pool, alice.id, Vec::new())
            .await
            .expect("no pairs is not a failure");
        assert!(written.is_empty(), "and no row is written");
        assert!(
            get_many(&pool, alice.id, &[HOME, NOTIFICATIONS])
                .await
                .expect("reading nothing is not a failure")
                .is_empty(),
            "the account still has no markers"
        );
    }

    #[tokio::test]
    async fn a_long_position_is_cut_at_two_hundred_fifty_five_characters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let over = "x".repeat(MAX_LAST_READ_ID_CHARS + 45);
        let written = upsert_many(&pool, alice.id, vec![(HOME, over.as_str())])
            .await
            .expect("an over-long position is stored, not refused");
        let stored = written.first().expect("the row").last_read_id.clone();
        assert_eq!(
            stored,
            "x".repeat(MAX_LAST_READ_ID_CHARS),
            "the first 255 characters, verbatim"
        );

        let wide = "…".repeat(MAX_LAST_READ_ID_CHARS + 10);
        let kept = upsert_many(&pool, alice.id, vec![(NOTIFICATIONS, wide.as_str())])
            .await
            .expect("multi-byte text upserts too")
            .into_iter()
            .next()
            .expect("one row");
        assert_eq!(
            kept.last_read_id.chars().count(),
            MAX_LAST_READ_ID_CHARS,
            "the cap counts characters"
        );
        assert!(
            kept.last_read_id.len() > MAX_LAST_READ_ID_CHARS,
            "and so it is more bytes than that"
        );
    }

    #[tokio::test]
    async fn get_many_reads_only_the_asked_for_timelines() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        mark(&pool, &alice, HOME, "h").await;
        mark(&pool, &alice, NOTIFICATIONS, "n").await;

        let one = get_many(&pool, alice.id, &[NOTIFICATIONS])
            .await
            .expect("one timeline reads");
        assert_eq!(timelines(&one), [NOTIFICATIONS], "only the one asked for");
        assert_eq!(one.first().expect("the row").last_read_id, "n");

        let none = get_many(&pool, alice.id, &[])
            .await
            .expect("an empty request reads");
        assert!(none.is_empty(), "and answers nothing at all");

        let unknown = get_many(&pool, alice.id, &["direct", HOME])
            .await
            .expect("an unknown timeline is not an error");
        assert_eq!(timelines(&unknown), [HOME], "it is simply absent");
    }

    #[tokio::test]
    async fn markers_are_each_account_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        mark(&pool, &alice, HOME, "alice-here").await;

        let bobs = get_many(&pool, bob.id, &[HOME])
            .await
            .expect("bob's read succeeds");
        assert!(bobs.is_empty(), "and finds nothing of alice's");

        mark(&pool, &bob, HOME, "bob-here").await;
        let alices = get_many(&pool, alice.id, &[HOME])
            .await
            .expect("alice's read succeeds");
        assert_eq!(
            alices.first().expect("her row").last_read_id,
            "alice-here",
            "bob's write left her marker alone"
        );
        assert_eq!(
            get_many(&pool, bob.id, &[HOME])
                .await
                .expect("bob reads again")
                .first()
                .expect("his row")
                .version,
            0,
            "and bob started at 0, not above alice's count"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_takes_their_markers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        mark(&pool, &alice, HOME, "h").await;
        mark(&pool, &alice, NOTIFICATIONS, "n").await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");

        let left = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM markers")
            .fetch_one(&pool)
            .await
            .expect("the markers count");
        assert_eq!(left, 0, "and both of their markers with it");
    }

    /// Inserts one `follow` notification of `recipient` by `actor`.
    async fn note(
        pool: &PgPool,
        recipient: &User,
        actor: &User,
    ) -> crate::models::notification::Notification {
        use crate::models::notification::{self, NewNotification};
        notification::create(
            pool,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "follow",
                status_id: None,
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture inserts")
    }

    /// [`unread_notifications_count`], unwrapped for the assertions.
    async fn count(pool: &PgPool, user_id: Uuid, last_read_id: &str) -> i64 {
        unread_notifications_count(pool, user_id, last_read_id)
            .await
            .expect("the count reads")
    }

    #[tokio::test]
    async fn the_marker_position_bounds_the_unread_notifications_count() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;

        assert_eq!(
            count(pool, alice.id, "").await,
            0,
            "with no rows at all the position does not matter"
        );

        let older = note(pool, &alice, &bob).await;
        let newer = note(pool, &alice, &bob).await;
        assert_eq!(
            count(pool, alice.id, "").await,
            2,
            "no position counts every unseen row"
        );
        assert_eq!(
            count(pool, alice.id, "n-1").await,
            2,
            "text that names no id is no position, Pleroma's skipped clause"
        );
        assert_eq!(
            count(pool, alice.id, &older.id.to_string()).await,
            1,
            "the marker itself is read; only newer rows count"
        );
        assert_eq!(
            count(pool, alice.id, &newer.id.to_string()).await,
            0,
            "and a marker at the newest row has nothing left"
        );
    }

    #[tokio::test]
    async fn seen_and_dismissed_notifications_leave_the_unread_count() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let older = note(pool, &alice, &bob).await;
        let newer = note(pool, &alice, &bob).await;

        let _seen = sqlx::query("UPDATE notifications SET seen = true WHERE id = $1")
            .bind(newer.id)
            .execute(pool)
            .await
            .expect("the seen flag lands");
        assert_eq!(
            count(pool, alice.id, "").await,
            1,
            "a seen row drops out of the count"
        );

        let _dismissed = sqlx::query("UPDATE notifications SET dismissed_at = now() WHERE id = $1")
            .bind(older.id)
            .execute(pool)
            .await
            .expect("the dismissal lands");
        assert_eq!(
            count(pool, alice.id, "").await,
            0,
            "and so does a dismissed one"
        );
    }

    #[tokio::test]
    async fn notifications_count_only_for_their_recipient() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let _first = note(pool, &alice, &bob).await;
        let _second = note(pool, &alice, &bob).await;

        assert_eq!(
            count(pool, bob.id, "").await,
            0,
            "the actor of alice's notifications has none of her own"
        );
        assert_eq!(count(pool, alice.id, "").await, 2, "alice holds both");
    }
}
