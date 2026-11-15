//! The `instances` table: the remote hosts this instance has heard of, and
//! the federation facts it accumulates about them.
//!
//! The reference is Pleroma's `Pleroma.Instance`
//! (`lib/pleroma/instance.ex`), whose `unreachable_since` column the
//! publisher consults through `Instances.filter_reachable/1` before it
//! addresses a host: a peer whose inbox has refused a delivery stops
//! receiving activities, which is what keeps a dead or defederated host from
//! soaking up the retries of every fan-out. [`is_host_unreachable`] is that
//! read, and the [`crate::federation::publisher`] gate runs it per candidate
//! inbox; [`touch`] records first sight of a host, [`set_unreachable`] marks
//! it, [`set_reachable`] clears the mark once a delivery works again, and
//! [`list_unreachable`] is the ledger itself, oldest mark first.
//!
//! The mark is a fact about *when a host went quiet*, so it never moves:
//! [`set_unreachable`] writes it only while the column is `NULL`, and
//! [`set_reachable`] is the only way back. A host with no row at all is
//! reachable — the ledger excludes nobody it has no evidence against.
//!
//! The window [`is_host_unreachable`] applies comes from
//! [`UNREACHABLE_GRACE_SECS`](crate::config::UNREACHABLE_GRACE_SECS_VAR)
//! ([`unreachable_grace_secs`](crate::config::Config::unreachable_grace_secs)),
//! not from the pool the lookup runs on, so the publisher passes it in: with
//! the shipped default of zero seconds any mark makes a host unreachable at
//! once, Pleroma's reading; a positive value lets a mark younger than the
//! window still deliver, giving a flaky peer a retry cycle before the whole
//! instance stops talking to it.
//!
//! The `favicon`, `favicon_updated_at` and `metadata_updated_at` columns are
//! schema-only in this ticket series: no favicon or node-metadata fetching is
//! specified yet, so nothing here writes them
//! (`docs/ARCHITECTURE.md` decision 15).
//!
//! [`relay_inboxes`] is the relay ticket's hook into this module: the
//! [`relays`](crate::models::relay) table owns the subscriptions, and this
//! is the read the publisher folds into its fan-out. Both it and
//! [`is_host_unreachable`] are plain `fn`s returning a [`BoxFuture`] rather
//! than `async` fns (the shape [`crate::federation::mrf::filter_incoming`]
//! set for exactly this reason): keeping one shape for both leaves every
//! call site in the publisher spelled the same. Both hand back an
//! [`AppResult`]: a database that cannot answer arrives as an error rather
//! than as a silently empty set.

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use sqlx::PgPool;

use crate::{error::AppResult, models::relay};

/// [`is_host_unreachable`]: the one fact the publisher needs, asked as an
/// existence check so an unmarked or never-seen host costs a `false` and not
/// a scanned table. The comparison runs against the database clock, so the
/// mark and the grace window age on one clock however many processes read
/// them; `$2` is the grace span as an `interval`.
const IS_UNREACHABLE_SQL: &str = "SELECT EXISTS (SELECT 1 FROM instances \
                                  WHERE host = $1 AND unreachable_since IS NOT NULL \
                                  AND unreachable_since < now() - $2::interval)";

/// [`touch`]: first sight of a host inserts its row, every other column at
/// its default; a host already known gets its `updated_at` bumped and nothing
/// else, so reseeing a peer never clears a mark or a cached fact.
const TOUCH_SQL: &str = "INSERT INTO instances (host) VALUES ($1) \
                         ON CONFLICT (host) DO UPDATE SET updated_at = now()";

/// [`set_unreachable`]: creates the row for a host seen for the first time
/// *as* unreachable; for a known host, `COALESCE` keeps the mark already
/// there, which is what makes an earlier failure's timestamp survive a later
/// one. `updated_at` says the row was written either way.
const SET_UNREACHABLE_SQL: &str = "INSERT INTO instances (host, unreachable_since) \
                                   VALUES ($1, $2) \
                                   ON CONFLICT (host) DO UPDATE SET \
                                   unreachable_since = COALESCE(instances.unreachable_since, \
                                                                 EXCLUDED.unreachable_since), \
                                   updated_at = now()";

/// [`set_reachable`]: the mark alone is cleared, and an `UPDATE` that matches
/// no row is the database's own no-op — a host with no row never had a mark.
const SET_REACHABLE_SQL: &str = "UPDATE instances SET unreachable_since = NULL, \
                                 updated_at = now() WHERE host = $1";

/// [`list_unreachable`]: the marked hosts, oldest mark first, so a ledger
/// read starts with the peers that have been quiet longest. The grace window
/// is not applied here — this answers what is marked, not what the publisher
/// currently skips.
const LIST_UNREACHABLE_SQL: &str = "SELECT host, unreachable_since FROM instances \
                                    WHERE unreachable_since IS NOT NULL \
                                    ORDER BY unreachable_since";

/// Whether `host` is marked unreachable for outbound federation.
///
/// True exactly when the host's row carries an `unreachable_since` mark
/// strictly older than `grace`; `grace` is
/// [`Config::unreachable_grace_secs`](crate::config::Config::unreachable_grace_secs)
/// as a span, and [`Duration::zero`](chrono::Duration::zero) — the shipped
/// default — makes any marked host unreachable the moment it is marked. A
/// host with no row, and a row with no mark, are both reachable.
///
/// `host` is compared exactly, so callers pass the canonical spelling the
/// rows hold: lowercase, no scheme and no port — what a URL's host part gives,
/// which is how [`crate::federation::publisher`] derives it before asking.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub fn is_host_unreachable<'p>(
    pool: &'p PgPool,
    host: &str,
    grace: chrono::Duration,
) -> BoxFuture<'p, AppResult<bool>> {
    // Owned for the returned future to keep: the [`BoxFuture`] is tied to the
    // pool's lifetime alone, exactly as the placeholder's was, so the call
    // site borrows nothing past its own `.await`.
    let host = host.to_owned();
    Box::pin(async move {
        let unreachable = sqlx::query_scalar::<_, bool>(IS_UNREACHABLE_SQL)
            .bind(host)
            .bind(grace)
            .fetch_one(pool)
            .await?;
        Ok(unreachable)
    })
}

/// Records first sight of `host`, or notes that a known host was seen again.
///
/// The insert leaves every other column at its default, so a host this
/// instance has merely heard of is reachable; the conflict arm writes
/// `updated_at` alone. Pleroma's `Instances.find_or_create/1` is the same
/// first-sight upsert.
///
/// # Errors
///
/// Any database failure (connection, constraint) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn touch(pool: &PgPool, host: &str) -> AppResult<()> {
    let _written = sqlx::query(TOUCH_SQL).bind(host).execute(pool).await?;
    Ok(())
}

/// Marks `host` unreachable as of `since`, creating its row when the host
/// has none.
///
/// The mark lands only while `unreachable_since` is still `NULL`: a second,
/// later failure never moves the first timestamp, because the column records
/// when a host *went* quiet, and the publisher's grace window counts from
/// there. Clearing is [`set_reachable`]'s job.
///
/// # Errors
///
/// Any database failure (connection, constraint) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_unreachable(pool: &PgPool, host: &str, since: DateTime<Utc>) -> AppResult<()> {
    let _written = sqlx::query(SET_UNREACHABLE_SQL)
        .bind(host)
        .bind(since)
        .execute(pool)
        .await?;
    Ok(())
}

/// Clears `host`'s unreachable mark: a peer that answered again is a delivery
/// target again.
///
/// A host with no row, and a row carrying no mark, are both a no-op rather
/// than an error — recovery is told about hosts that may never have been
/// marked at all.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_reachable(pool: &PgPool, host: &str) -> AppResult<()> {
    let _cleared = sqlx::query(SET_REACHABLE_SQL)
        .bind(host)
        .execute(pool)
        .await?;
    Ok(())
}

/// Every host carrying a mark, oldest mark first, with the time it was
/// marked.
///
/// Unmarked rows — hosts merely seen, or recovered by
/// [`set_reachable`] — are not listed, and no grace window is applied: this is
/// the ledger, the input to [`is_host_unreachable`]'s decision rather than its
/// answer.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_unreachable(pool: &PgPool) -> AppResult<Vec<(String, DateTime<Utc>)>> {
    let rows = sqlx::query_as::<_, (String, DateTime<Utc>)>(LIST_UNREACHABLE_SQL)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// [`delete`]: the withdrawal of the ledger entry itself; whether a row
/// stood there is the answer.
const DELETE_SQL: &str = "DELETE FROM instances WHERE host = $1";

/// Removes `host`'s row from the ledger, reporting whether one stood
/// there.
///
/// This is the instance-deletion act of
/// `DELETE /api/v1/pleroma/admin/instances/{instance}`
/// ([`crate::api::admin::federation`]), which clears the host's mirrored
/// accounts through [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)
/// first. Unknown hosts are the false answer, not an error — the admin
/// deletion of a never-seen domain is a legal no-op, exactly as Pleroma's
/// worker's `nil -> :ok` arm reads it.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, host: &str) -> AppResult<bool> {
    let gone = sqlx::query(DELETE_SQL).bind(host).execute(pool).await?;
    Ok(gone.rows_affected() > 0)
}

/// The inbox URLs of the relays this instance follows *and that have
/// accepted the follow*.
///
/// Pleroma's `Relay.followed_inboxes/0`, the set the
/// [publisher](crate::federation::publisher) adds for public
/// `Create`/`Announce`/`Update`/`Delete` activities. A `pending`
/// subscription has not been granted yet, so its inbox joins the set only
/// when the relay's `Accept` folds
/// ([`mark_accepted`](crate::models::relay::mark_accepted)).
pub fn relay_inboxes(pool: &PgPool) -> BoxFuture<'_, AppResult<Vec<String>>> {
    Box::pin(relay::accepted_inboxes(pool))
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone as _};

    use super::*;

    /// [`row`]: the three columns a test cares about, in order.
    const ROW_SQL: &str = "SELECT inserted_at, updated_at, unreachable_since \
                           FROM instances WHERE host = $1";
    /// The grace span that means "any mark counts": the shipped default of
    /// `UNREACHABLE_GRACE_SECS`.
    const NO_GRACE: Duration = Duration::zero();
    /// The host every single-host test here is about.
    const HOST: &str = "peer.example";

    /// One fixed moment in June 2025 — a whole minute, so the microsecond
    /// precision the `timestamptz` columns keep round-trips it unchanged and
    /// a test comparing a mark it wrote compares exactly. Every one of them
    /// is in the past, the only position a real mark can sit in.
    fn stamp(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 6, day, hour, 0, 0)
            .single()
            .expect("a real minute in June 2025")
    }

    /// One host's row: `inserted_at`, `updated_at` and its mark.
    async fn row(
        pool: &PgPool,
        host: &str,
    ) -> (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>) {
        sqlx::query_as::<_, (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>)>(ROW_SQL)
            .bind(host)
            .fetch_one(pool)
            .await
            .expect("the host has a row")
    }

    /// Rewrites a row's two bookkeeping stamps to `at` directly, so the next
    /// write's bump is an observable move rather than a race with the clock.
    async fn age_stamps(pool: &PgPool, host: &str, at: DateTime<Utc>) {
        let aged =
            sqlx::query("UPDATE instances SET inserted_at = $2, updated_at = $2 WHERE host = $1")
                .bind(host)
                .bind(at)
                .execute(pool)
                .await
                .expect("the stamps are settable");
        assert_eq!(aged.rows_affected(), 1, "the row is there to age");
    }

    /// Whether `host` reads unreachable under the default zero window.
    async fn unreachable(pool: &PgPool, host: &str) -> bool {
        is_host_unreachable(pool, host, NO_GRACE)
            .await
            .expect("the lookup runs")
    }

    /// The mark `host`'s row carries, `None` when it carries none.
    async fn mark_of(pool: &PgPool, host: &str) -> Option<DateTime<Utc>> {
        row(pool, host).await.2
    }

    /// A relay subscription hands its inbox to the publisher only once the
    /// relay has accepted the follow: a `pending` row is not a fan-out
    /// target, and the grant is what opens one.
    #[tokio::test]
    async fn relay_inboxes_answers_the_accepted_subscriptions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            relay_inboxes(&pool)
                .await
                .expect("the lookup runs")
                .is_empty(),
            "no subscriptions, no relay inboxes"
        );

        let _waiting = relay::upsert(
            &pool,
            "https://waiting.example/relay",
            "https://waiting.example/inbox",
        )
        .await
        .expect("the pending subscription stores");
        let _granted_row = relay::upsert(
            &pool,
            "https://granted.example/relay",
            "https://granted.example/inbox",
        )
        .await
        .expect("the second subscription stores");
        let _moved = relay::mark_accepted(&pool, "https://granted.example/relay")
            .await
            .expect("the grant runs");

        let inboxes = relay_inboxes(&pool).await.unwrap();
        assert_eq!(
            inboxes,
            vec!["https://granted.example/inbox".to_owned()],
            "the accepted relay only, until the other one answers"
        );

        let _waiting_granted = relay::mark_accepted(&pool, "https://waiting.example/relay")
            .await
            .expect("the second grant runs");
        // Age the first subscription so the ordering it expects is settled
        // rather than a race against the microsecond clock.
        let _aged = sqlx::query(
            "UPDATE relays SET inserted_at = now() - interval '1 minute' WHERE actor_ap_id = $1",
        )
        .bind("https://waiting.example/relay")
        .execute(&pool)
        .await
        .expect("the stamp is settable");
        let both = relay_inboxes(&pool).await.unwrap();
        assert_eq!(
            both,
            vec![
                "https://waiting.example/inbox".to_owned(),
                "https://granted.example/inbox".to_owned(),
            ],
            "and both subscriptions deliver, oldest follow first"
        );
    }

    /// A host with no row is reachable, and a row only [`touch`] has ever
    /// written carries no mark: first sight is not evidence.
    #[tokio::test]
    async fn an_unseen_host_and_a_touched_one_are_both_reachable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(!unreachable(&pool, HOST).await, "no row means no evidence");

        touch(&pool, HOST).await.unwrap();
        let (inserted, updated, mark) = row(&pool, HOST).await;
        assert!(mark.is_none(), "a touched host carries no mark");
        assert_eq!(
            inserted, updated,
            "the first-sight row is stamped once, by its own insert"
        );
        assert!(
            !unreachable(&pool, HOST).await,
            "and the touched host is still reachable"
        );
    }

    /// [`touch`] on a host it already knows writes `updated_at` and nothing
    /// else: the mark stays, and `inserted_at` keeps saying first sight.
    #[tokio::test]
    async fn touch_on_conflict_only_bumps_updated_at() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first_sight = stamp(1, 8);
        let mark = stamp(4, 12);

        // A host seen long ago that went quiet later.
        set_unreachable(&pool, HOST, mark).await.unwrap();
        age_stamps(&pool, HOST, first_sight).await;

        touch(&pool, HOST).await.unwrap();
        let (inserted, updated, now_mark) = row(&pool, HOST).await;
        assert_eq!(inserted, first_sight, "first sight is never rewritten");
        assert!(updated > first_sight, "the second sight bumps updated_at");
        assert_eq!(
            now_mark,
            Some(mark),
            "and touching a host leaves its mark alone"
        );
        assert!(unreachable(&pool, HOST).await, "so it is still unreachable");
    }

    /// [`set_unreachable`] creates the missing row, and a later mark never
    /// overwrites the earlier one: the column says when the host went quiet.
    #[tokio::test]
    async fn set_unreachable_keeps_the_first_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = stamp(1, 0);
        let later = stamp(3, 0);

        // The host has no row at all: the mark creates it.
        set_unreachable(&pool, HOST, first).await.unwrap();
        assert_eq!(
            mark_of(&pool, HOST).await,
            Some(first),
            "the absent row was created marked"
        );

        set_unreachable(&pool, HOST, later).await.unwrap();
        assert_eq!(
            mark_of(&pool, HOST).await,
            Some(first),
            "a second, newer failure keeps the first mark"
        );
        assert!(
            unreachable(&pool, HOST).await,
            "and the host is unreachable"
        );

        // A host that was touched first takes the mark the same way, and then
        // holds it against a third failure.
        touch(&pool, "other.example").await.unwrap();
        set_unreachable(&pool, "other.example", later)
            .await
            .unwrap();
        set_unreachable(&pool, "other.example", stamp(5, 0))
            .await
            .unwrap();
        assert_eq!(
            mark_of(&pool, "other.example").await,
            Some(later),
            "the first mark on a touched host stands"
        );
    }

    /// [`set_reachable`] clears the mark, and is a silent no-op for a host the
    /// ledger has never heard of.
    #[tokio::test]
    async fn set_reachable_clears_a_mark_and_ignores_a_stranger() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_unreachable(&pool, HOST, stamp(1, 0)).await.unwrap();
        assert!(unreachable(&pool, HOST).await, "marked first");

        set_reachable(&pool, HOST).await.unwrap();
        assert_eq!(
            mark_of(&pool, HOST).await,
            None,
            "the mark is gone, the row stays"
        );
        assert!(
            !unreachable(&pool, HOST).await,
            "and the recovered host delivers again"
        );

        set_reachable(&pool, "stranger.example")
            .await
            .expect("an unknown host is a no-op, not an error");
        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
            .bind("stranger.example")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "clearing an unseen host creates nothing");
        assert!(
            !unreachable(&pool, "stranger.example").await,
            "and the stranger stays reachable"
        );
    }

    /// The states a host moves through: never seen, touched, unreachable,
    /// recovered, unreachable again — the last mark starting fresh because
    /// the clear left the column `NULL`.
    #[tokio::test]
    async fn the_reachability_state_transitions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = stamp(1, 0);
        let second = stamp(2, 0);
        let third = stamp(3, 0);

        assert!(!unreachable(&pool, HOST).await, "a never-seen host");
        touch(&pool, HOST).await.unwrap();
        assert!(!unreachable(&pool, HOST).await, "a seen host");

        set_unreachable(&pool, HOST, first).await.unwrap();
        assert!(unreachable(&pool, HOST).await, "a marked host");
        set_unreachable(&pool, HOST, second).await.unwrap();
        assert_eq!(mark_of(&pool, HOST).await, Some(first), "and holding");

        set_reachable(&pool, HOST).await.unwrap();
        assert!(!unreachable(&pool, HOST).await, "a recovered host");

        set_unreachable(&pool, HOST, third).await.unwrap();
        assert_eq!(
            mark_of(&pool, HOST).await,
            Some(third),
            "a new quiet spell starts a new mark"
        );
        assert!(
            unreachable(&pool, HOST).await,
            "and it is unreachable again"
        );
    }

    /// The grace window: a mark younger than `grace` reads reachable, an
    /// older one reads unreachable, and the zero default lets no mark
    /// through. The marks here are relative to the clock, because this is the
    /// only test whose answer depends on how old the mark is.
    #[tokio::test]
    async fn the_grace_window_decides_a_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_unreachable(&pool, HOST, Utc::now() - Duration::hours(1))
            .await
            .unwrap();

        assert!(
            !is_host_unreachable(&pool, HOST, Duration::hours(2))
                .await
                .unwrap(),
            "a mark younger than the grace window is still deliverable"
        );
        assert!(
            is_host_unreachable(&pool, HOST, Duration::minutes(30))
                .await
                .unwrap(),
            "once the mark outlives the window the host is unreachable"
        );
        assert!(
            unreachable(&pool, HOST).await,
            "and zero grace takes any mark"
        );

        // The window only ever spares a host that carries no mark anyway.
        touch(&pool, "clean.example").await.unwrap();
        for grace in [NO_GRACE, Duration::hours(2)] {
            assert!(
                !is_host_unreachable(&pool, "clean.example", grace)
                    .await
                    .unwrap(),
                "an unmarked host is reachable at grace {grace}"
            );
        }
    }

    /// A mark written now is unreachable now under the zero default: the
    /// semantics `UNREACHABLE_GRACE_SECS=0` promises, with no back-off to
    /// wait out.
    #[tokio::test]
    async fn the_default_window_takes_a_fresh_mark_at_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_unreachable(&pool, HOST, Utc::now()).await.unwrap();
        assert!(
            unreachable(&pool, HOST).await,
            "zero grace means the mark counts with the write that made it"
        );
        assert!(
            !is_host_unreachable(&pool, HOST, Duration::hours(1))
                .await
                .unwrap(),
            "and the same fresh mark is a young one for any positive window"
        );
    }

    /// [`list_unreachable`] answers only the marked hosts, oldest mark first,
    /// with the time each was marked, and drops a host the moment it
    /// recovers.
    #[tokio::test]
    async fn list_unreachable_answers_the_marked_hosts_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        touch(&pool, "clean.example").await.unwrap();
        set_reachable(&pool, "never-seen.example").await.unwrap();
        set_unreachable(&pool, "newest.example", stamp(5, 0))
            .await
            .unwrap();
        set_unreachable(&pool, "oldest.example", stamp(1, 0))
            .await
            .unwrap();
        set_unreachable(&pool, "middle.example", stamp(3, 0))
            .await
            .unwrap();

        let listed = list_unreachable(&pool).await.unwrap();
        assert_eq!(
            listed,
            vec![
                ("oldest.example".to_owned(), stamp(1, 0)),
                ("middle.example".to_owned(), stamp(3, 0)),
                ("newest.example".to_owned(), stamp(5, 0)),
            ],
            "the marked hosts only, oldest mark first, with their since-values"
        );

        set_reachable(&pool, "middle.example").await.unwrap();
        let after = list_unreachable(&pool).await.unwrap();
        assert_eq!(
            after,
            vec![
                ("oldest.example".to_owned(), stamp(1, 0)),
                ("newest.example".to_owned(), stamp(5, 0)),
            ],
            "a recovered host leaves the ledger, and the order holds"
        );
    }

    /// The primary key holds one row per host, so every write here is an
    /// upsert against the same row rather than another one.
    #[tokio::test]
    async fn one_row_per_host_holds_across_every_write() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        touch(&pool, HOST).await.unwrap();
        set_unreachable(&pool, HOST, stamp(1, 0)).await.unwrap();
        touch(&pool, HOST).await.unwrap();
        set_reachable(&pool, HOST).await.unwrap();
        set_unreachable(&pool, HOST, stamp(2, 0)).await.unwrap();

        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
            .bind(HOST)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "five writes, one row");
    }

    /// [`delete`] takes the ledger row with it and reports whether one
    /// stood there: a marked host withdraws once, a repeat and a never-
    /// seen host are silent no-ops, and no other host's row is touched.
    #[tokio::test]
    async fn delete_withdraws_the_ledger_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_unreachable(&pool, HOST, stamp(1, 0)).await.unwrap();
        touch(&pool, "other.example").await.unwrap();

        assert!(delete(&pool, HOST).await.unwrap(), "the row was there");
        assert!(
            !unreachable(&pool, HOST).await,
            "the deleted host is gone from the ledger entirely"
        );
        assert!(
            !delete(&pool, HOST).await.unwrap(),
            "a repeat finds nothing to delete"
        );
        assert!(
            !delete(&pool, "never-seen.example").await.unwrap(),
            "an unknown host is a no-op, not an error"
        );

        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
            .bind(HOST)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "no row stands behind the deleted host");
        let kept = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
            .bind("other.example")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 1, "and the neighbouring row is untouched");
    }
}
