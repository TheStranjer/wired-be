//! The `mutes` table: which account hides another from its timelines, with
//! which options, and until when.
//!
//! The reference behaviour is Pleroma's `User.mute` / `User.unmute`
//! (`lib/pleroma/user.ex`) over the mute edge of `Pleroma.UserRelationship`
//! (`lib/pleroma/user_relationship.ex`), reached from the Mastodon
//! `AccountController`'s `mute`/`unmute`/`mutes` actions
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`). Where
//! Pleroma splits one mute across two edges (the timeline mute and the
//! notification mute), this table keeps the fact as one row per pair with
//! Mastodon's two parameters as its columns: `hide_notifications` is the
//! `notifications` parameter (`true` hides the muted account's notifications
//! as well as its posts, `false` mutes the timeline only) and `expires_at` is
//! its `duration`, counted from the write, `NULL` standing for an indefinite
//! mute.
//!
//! [`mute`] is an upsert on the ticket's `(muter_id, muted_id)` key: a pair
//! that is already muted keeps its row — and its id, so the listing's cursor
//! does not move, and its `inserted_at` — while the two options are rewritten
//! from the request, so re-muting is how a client changes or clears a
//! duration, or revives a mute that lapsed but has not been swept yet.
//! [`unmute`] deletes whatever pair it is given and is content with none.
//! Deleting either account takes its mutes through the foreign key's cascade.
//! Unlike a block, a mute severs nothing: the pair's follows stay exactly as
//! they were, the way Pleroma's `User.mute` writes its edges and nothing else.
//!
//! A mute at or past its `expires_at` is treated as absent by *every* read
//! here — each statement carries `expires_at IS NULL OR expires_at > now()`,
//! asked against the database clock so the instant a mute stops applying is
//! the same one every connection sees — and [`delete_expired`] removes those
//! rows for good; the `mutes.expire` periodic job runs it. Until it does, the
//! filters answer as though the mute had been lifted, which is what the
//! timeline and notification filters need.
//!
//! The reads are the ones the endpoints, the [`Relationship`] loader and the
//! filters need: [`is_muting`] the one directional fact, [`muted_ids`] and
//! [`notification_muted_ids`] the flat sets a timeline or an inbox filters by,
//! and [`muting_page`] the `GET /api/v1/mutes` listing, each entry led by the
//! [`Mute`] row next to the [`User`] behind it — the row id is what the cursor
//! and the `Link` header key on (decision 4), and the row's
//! [`expires_at`](Mute::expires_at) is what the listing renders as
//! `mute_expires_at`.
//!
//! [`Relationship`]: crate::entities::relationship::Relationship

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::User,
    pagination::PageParams,
    util::id::new_id,
};

/// 422 message [`mute`] answers when an account asks to mute itself, the
/// wording this ticket pins for the endpoint (Pleroma's changeset refuses the
/// self relationship for every edge type the same way).
const SELF_MUTE: &str = "Can not mute yourself";

/// [`mute`]: the pair the caller names, with its `expires_at` computed by the
/// database clock from the bound duration (see [`expiry_of`]) — and `NULL` for
/// an indefinite mute, because `now() + NULL::interval` is `NULL`, so this one
/// statement spells both timed and indefinite writes.
/// `ON CONFLICT (muter_id, muted_id) DO UPDATE` is the ticket's upsert: a pair
/// that is already muted keeps its row, its id and its `inserted_at`, and
/// takes the two options from the new write. `RETURNING *` brings whichever row
/// the statement wrote back to the caller.
const UPSERT_SQL: &str =
    "INSERT INTO mutes (id, muter_id, muted_id, hide_notifications, expires_at)
     VALUES ($1, $2, $3, $4, now() + $5::interval)
ON CONFLICT (muter_id, muted_id) DO UPDATE
        SET hide_notifications = EXCLUDED.hide_notifications,
            expires_at = EXCLUDED.expires_at
RETURNING *";

/// [`unmute`]: the pair's row, which comes back so the caller can report
/// whether a mute was there to lift. A never-muted (or already lifted) pair
/// deletes nothing, so unmuting is idempotent. Note that a lapsed mute's row
/// is still there to delete: reads treat it as absent, this statement removes
/// it without waiting for the sweep.
const UNMUTE_SQL: &str = "DELETE FROM mutes WHERE muter_id = $1 AND muted_id = $2
  RETURNING *";

/// [`is_muting`]: the one directional fact, asked as an existence check so a
/// muter never costs more than a `true`. The expiry clause is what makes a
/// lapsed mute answer `false` before the sweep has deleted its row.
const IS_MUTING_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM mutes
     WHERE muter_id = $1 AND muted_id = $2
       AND (expires_at IS NULL OR expires_at > now())
)";

/// [`muted_ids`]: every account one muter still mutes, oldest mute first (the
/// `UUIDv7` text sorts in creation order, decision 4). The order is a
/// stability promise, not semantics: callers filter by membership and page
/// through [`muting_page`].
const MUTED_IDS_SQL: &str = "SELECT muted_id FROM mutes
      WHERE muter_id = $1 AND (expires_at IS NULL OR expires_at > now())
   ORDER BY id";

/// [`notification_muted_ids`]: the still-muted accounts whose notifications
/// the muter also hides — [`MUTED_IDS_SQL`]'s set narrowed to the rows whose
/// `hide_notifications` column is true. An indefinite mute counts; an expired
/// one does not.
const NOTIFICATION_MUTED_IDS_SQL: &str = "SELECT muted_id FROM mutes
      WHERE muter_id = $1 AND hide_notifications
        AND (expires_at IS NULL OR expires_at > now())
   ORDER BY id";

/// The leading text of [`muting_page`]: the still-active muted accounts of one
/// muter, each entry led by the columns of the `mutes` row that put them on
/// the page (aliased where [`User`] has a column of the same name: `id` and
/// `inserted_at`), joined to the account on the other end. The expiry filter
/// comes before the muter's bind because [`PageParams::apply_keyset`] appends
/// its own conditions after it, and the statement stops where that placeholder
/// belongs: the builder is what numbers it `$1`, and the keyset runs on the
/// mute row's own `id`.
const PAGE_SQL: &str = "SELECT m.id AS mute_id, m.muter_id, m.muted_id,
                               m.hide_notifications, m.expires_at,
                               m.inserted_at AS mute_inserted_at, u.*
                          FROM mutes m
                          JOIN users u ON u.id = m.muted_id
                         WHERE (m.expires_at IS NULL OR m.expires_at > now())
                           AND m.muter_id = ";

/// [`delete_expired`]: every mute whose `expires_at` has been reached, i.e.
/// every row the reads already treat as absent. Rows at exactly `now()` go
/// too: `expires_at > now()` is what keeps a mute, so a row failing that
/// comparison has expired.
const DELETE_EXPIRED_SQL: &str =
    "DELETE FROM mutes WHERE expires_at IS NOT NULL AND expires_at <= now()";

/// One row of `mutes`: the `muter_id` account hides the `muted_id` one, with
/// the two Mastodon options the write was given.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Mute {
    /// `UUIDv7` primary key, minted by [`mute`] (decision 4); the id
    /// `GET /api/v1/mutes` pages by.
    pub id: Uuid,
    /// The muting account; every read and write of a mute is scoped to it.
    pub muter_id: Uuid,
    /// The muted account.
    pub muted_id: Uuid,
    /// Mastodon's `notifications` parameter as stored: `true` hides the muted
    /// account's notifications along with its posts, `false` mutes only the
    /// timeline. The [`muting_notifications`] flag of the [`Relationship`]
    /// entity reads it.
    ///
    /// [`muting_notifications`]: crate::entities::relationship::Relationship::muting_notifications
    /// [`Relationship`]: crate::entities::relationship::Relationship
    pub hide_notifications: bool,
    /// When the mute stops applying, `None` for an indefinite one. A row at or
    /// past this instant answers as absent to every read here until
    /// [`delete_expired`] removes it.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the row was first inserted. A re-mute rewrites the options of the
    /// same row and leaves this — and the row's [`id`](Self::id) — alone.
    pub inserted_at: DateTime<Utc>,
}

/// The options a mute is written with: the two parameters
/// `POST /api/v1/accounts/{id}/mute` takes, already resolved to what they mean
/// for the row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MuteOptions {
    /// Mastodon's `notifications`: whether the mute also hides the muted
    /// account's notifications. The parameter's own default is `true`.
    pub notifications: bool,
    /// Mastodon's `duration`, in seconds counted from the write: `Some(secs)`
    /// with `secs` positive mutes for that long; `None` — and any non-positive
    /// value — mutes indefinitely.
    pub expires_in: Option<i64>,
}

impl Default for MuteOptions {
    /// Mastodon's defaults for both parameters: notifications hidden, no
    /// duration, i.e. an ordinary indefinite mute.
    fn default() -> Self {
        Self {
            notifications: true,
            expires_in: None,
        }
    }
}

/// One fetched row of [`muting_page`]: the [`Mute`] columns the page keys on
/// next to the [`User`] they point at, read off [`PAGE_SQL`]'s join and folded
/// into that pair by [`into_entry`].
#[derive(Debug, sqlx::FromRow)]
struct MuteUserRow {
    /// Primary key of the `mutes` row behind this entry, aliased because the
    /// joined account row carries its own `id`.
    mute_id: Uuid,
    /// The muter: `mutes.muter_id`, the account the page is scoped to.
    muter_id: Uuid,
    /// The muted account: `mutes.muted_id`, the user this entry is about.
    muted_id: Uuid,
    /// Stored `hide_notifications` column of the mute row, unaliased because
    /// [`User`] has no column of that name.
    hide_notifications: bool,
    /// Stored `expires_at` of the mute row, likewise unaliased; the value the
    /// listing renders as `mute_expires_at`.
    expires_at: Option<DateTime<Utc>>,
    /// The mute's `inserted_at`, aliased because the user row carries its own
    /// creation timestamp under that name.
    mute_inserted_at: DateTime<Utc>,
    /// The muted account, decoded from the join's unaliased `u.*`.
    #[sqlx(flatten)]
    user: User,
}

/// Records that `muter` mutes `target` with `options`, and returns the row.
///
/// The write is an upsert on the pair: a first mute of a pair mints the row's
/// id (decision 4) and stores both options; a repeat of a pair that is already
/// muted — or whose mute has lapsed but is still on the books — keeps the row
/// and its id and rewrites the two options from `options`. That makes re-muting
/// the way a client extends a timed mute, clears one by muting again with no
/// duration, or revives a lapsed mute.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not mute yourself"` when the two
/// users are the same account. Any database failure (connection, constraint,
/// encoding) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mute(
    pool: &PgPool,
    muter: &User,
    target: &User,
    options: MuteOptions,
) -> AppResult<Mute> {
    if muter.id == target.id {
        return Err(AppError::Unprocessable(SELF_MUTE.to_owned()));
    }
    let row = sqlx::query_as::<_, Mute>(UPSERT_SQL)
        .bind(new_id())
        .bind(muter.id)
        .bind(target.id)
        .bind(options.notifications)
        .bind(expiry_of(options))
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Lifts `muter`'s mute on `target`, reporting the removed row.
///
/// Unmuting restores nothing and requires nothing: a pair that was never muted
/// (or whose mute was already lifted) is the `None` answer and changes
/// nothing, so the write is idempotent. A lapsed mute whose row the sweep has
/// not yet deleted is a row like any other, so lifting it reports that row even
/// though no read counts it as a mute anymore.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unmute(pool: &PgPool, muter: &User, target: &User) -> AppResult<Option<Mute>> {
    let removed = sqlx::query_as::<_, Mute>(UNMUTE_SQL)
        .bind(muter.id)
        .bind(target.id)
        .fetch_optional(pool)
        .await?;
    Ok(removed)
}

/// Whether `muter` currently mutes `target`.
///
/// The one directional fact the timeline and notification filters ask for. A
/// mute at or past its [`Mute::expires_at`] is not there to find, and neither
/// is a pair that was never muted — including one account with itself, which
/// has no row to find because [`mute`] refuses to write it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_muting(pool: &PgPool, muter: Uuid, target: Uuid) -> AppResult<bool> {
    let row = sqlx::query_scalar::<_, bool>(IS_MUTING_SQL)
        .bind(muter)
        .bind(target)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The ids of the accounts `muter` still mutes, oldest mute first.
///
/// The whole set in one statement: [`crate::models::relationship`] answers
/// `muting` from the same table, and the timeline filters that hide a muted
/// account's posts take this flat form. Expired mutes are absent from it, and a
/// muter with nothing muted answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn muted_ids(pool: &PgPool, muter: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(MUTED_IDS_SQL)
        .bind(muter)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(muted_id,)| muted_id).collect())
}

/// The ids of the accounts `muter` still mutes *and* hides the notifications
/// of, oldest mute first.
///
/// [`muted_ids`]' set narrowed to the rows written with Mastodon's
/// `notifications` parameter left `true`: what the notification filters drop
/// and the [`Relationship`] entity's `muting_notifications` flag reads. An
/// account muted for its timeline only is in [`muted_ids`] and not here, an
/// expired mute is in neither, and a muter hiding nothing answers the empty
/// `Vec`.
///
/// [`Relationship`]: crate::entities::relationship::Relationship
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn notification_muted_ids(pool: &PgPool, muter: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(NOTIFICATION_MUTED_IDS_SQL)
        .bind(muter)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(muted_id,)| muted_id).collect())
}

/// The accounts `muter` still mutes, newest mute first and `page`'s `limit` at
/// a time, each entry the [`Mute`] row next to the [`User`] it names.
///
/// Like the relationship pages, the cursor and the `Link` header key on the
/// *mute row* id and not on the accounts (decision 4), which is why the row
/// comes back beside the user — and why the listing can render the row's
/// expiry as `mute_expires_at`. An expired mute is on no page.
/// [`PageParams::finish`] flips a `min_id` fetch back to newest-first, so every
/// page is answered newest mute first.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn muting_page(
    pool: &PgPool,
    muter: Uuid,
    page: PageParams,
) -> AppResult<Vec<(Mute, User)>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(muter);
    page.apply_keyset(&mut builder, "m.id");
    let rows = builder
        .build_query_as::<MuteUserRow>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows).into_iter().map(into_entry).collect())
}

/// Deletes every mute whose `expires_at` has been reached and reports how many
/// rows went.
///
/// The sweep the `mutes.expire` periodic job runs: the reads already treat
/// these rows as absent, so deleting them is housekeeping, not the moment a
/// mute stops. An indefinite mute has no `expires_at` and is never touched.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_expired(pool: &PgPool) -> AppResult<u64> {
    let removed = sqlx::query(DELETE_EXPIRED_SQL).execute(pool).await?;
    Ok(removed.rows_affected())
}

/// The interval [`UPSERT_SQL`] adds to the database's `now()`: the requested
/// duration as a [`Duration`], or `None` — which the statement turns into a
/// `NULL` `expires_at` — for an indefinite mute. No duration at all counts, and
/// so does one that is not positive: Mastodon's `duration=0` is "no expiry",
/// and a nonsense negative value is treated the same way rather than as a mute
/// that is already over.
fn expiry_of(options: MuteOptions) -> Option<Duration> {
    options
        .expires_in
        .filter(|secs| *secs > 0)
        .and_then(|secs| u64::try_from(secs).ok())
        .map(Duration::from_secs)
}

/// Splits [`MuteUserRow`] back into the pair [`muting_page`] answers: the
/// [`Mute`] of the aliased columns, next to the [`User`] the join flattened.
fn into_entry(row: MuteUserRow) -> (Mute, User) {
    let mute = Mute {
        id: row.mute_id,
        muter_id: row.muter_id,
        muted_id: row.muted_id,
        hide_notifications: row.hide_notifications,
        expires_at: row.expires_at,
        inserted_at: row.mute_inserted_at,
    };
    (mute, row.user)
}

#[cfg(test)]
mod tests {
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, insert, local_ap_id},
        pagination::MAX_LIMIT,
        util::time::mastodon_time,
    };

    /// The options of the timeline-only mute the notification checks use.
    const TIMELINE_ONLY: MuteOptions = MuteOptions {
        notifications: false,
        expires_in: None,
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

    /// Mutes `target` for `muter` with `options`, checks the row it wrote and
    /// hands it back: the fixture a read below starts from and asserts on.
    async fn write_mute(pool: &PgPool, muter: &User, target: &User, options: MuteOptions) -> Mute {
        let row = mute(pool, muter, target, options)
            .await
            .expect("the fixture mute stores");
        assert_eq!(
            row.muted_id, target.id,
            "the row is the pair that was asked for"
        );
        row
    }

    /// Mutes `target` for `muter` with `options` and forgets the row: the
    /// fixture a test only needs to have happened.
    async fn seed_mute(pool: &PgPool, muter: &User, target: &User, options: MuteOptions) {
        let row = write_mute(pool, muter, target, options).await;
        assert!(
            is_muting(pool, muter.id, row.muted_id).await.unwrap(),
            "the fixture mute reads back as a mute"
        );
    }

    /// The stored mute of the pair, which the test expects to exist.
    async fn stored_mute(pool: &PgPool, muter: &User, target: &User) -> Mute {
        let sql = "SELECT * FROM mutes WHERE muter_id = $1 AND muted_id = $2";
        sqlx::query_as::<_, Mute>(sql)
            .bind(muter.id)
            .bind(target.id)
            .fetch_optional(pool)
            .await
            .expect("the read works")
            .expect("the mute exists")
    }

    /// Writes `target` into `muter`'s mute list with `expires_at` set to
    /// `seconds` from now (negative for the past), bypassing [`mute`]: the
    /// state the sweep and the expiry filters start from, which [`mute`] cannot
    /// produce because it always computes the column from the write's `now()`.
    async fn mute_expiring_in(pool: &PgPool, muter: &User, target: &User, seconds: i64) {
        let sql = "INSERT INTO mutes (id, muter_id, muted_id, expires_at)
                    VALUES ($1, $2, $3, now() + make_interval(secs => $4))";
        let written = sqlx::query(sql)
            .bind(new_id())
            .bind(muter.id)
            .bind(target.id)
            .bind(seconds)
            .execute(pool)
            .await
            .expect("the lapsed fixture row inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
    }

    /// Every row of the table, newest mute first — the order a page walks,
    /// read straight from storage so same-millisecond `UUIDv7`s cannot make an
    /// expectation depend on the order the fixtures were written in.
    async fn stored_rows(pool: &PgPool) -> Vec<(Uuid, Uuid, Uuid)> {
        sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
            "SELECT id, muter_id, muted_id FROM mutes ORDER BY id DESC",
        )
        .fetch_all(pool)
        .await
        .expect("the rows read")
    }

    /// The number of rows in the table.
    async fn mute_count(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM mutes")
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    /// Page params built from a query string, the way the extractor makes them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    /// How many seconds ahead of `now()` a stored `expires_at` lies, refusing
    /// to answer for a mute that has none: the write takes its own instant, so
    /// a test asserts a window and not an exact timestamp.
    fn seconds_ahead(row: &Mute) -> i64 {
        let expiry = row.expires_at.expect("a timed mute stores an expiry");
        expiry.signed_duration_since(Utc::now()).num_seconds()
    }

    /// The muted ids of `entries`, in the order they came back.
    fn muted_ids_of(entries: &[(Mute, User)]) -> Vec<Uuid> {
        entries.iter().map(|(_, user)| user.id).collect()
    }

    /// The mute row ids of `entries`, in the order they came back.
    fn row_ids_of(entries: &[(Mute, User)]) -> Vec<Uuid> {
        entries.iter().map(|(row, _)| row.id).collect()
    }

    /// The SQLSTATE a database failure carries, or an empty string when the
    /// failure is not the database reporting a constraint.
    fn sqlstate_of(err: SqlxError) -> String {
        if let SqlxError::Database(db_err) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_mute_is_written_and_read_back(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = write_mute(&pool, &alice, &bob, MuteOptions::default()).await;

        assert!(row.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(row.muter_id, alice.id, "who mutes");
        assert_eq!(row.muted_id, bob.id, "who is muted");
        assert!(
            row.hide_notifications,
            "the default mute hides notifications too"
        );
        assert!(row.expires_at.is_none(), "and defaults to indefinite");
        assert!(row.inserted_at <= Utc::now(), "the database stamps it");
        assert_eq!(
            stored_mute(&pool, &alice, &bob).await,
            row,
            "the row reads back as it was written"
        );

        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "alice mutes bob"
        );
        assert!(
            !is_muting(&pool, bob.id, alice.id).await.unwrap(),
            "muting is directional"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "bob is on alice's list"
        );
        assert!(
            muted_ids(&pool, bob.id).await.unwrap().is_empty(),
            "and alice is on nobody's"
        );
        assert_eq!(
            notification_muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and bob's notifications are hidden too"
        );

        let entries = muting_page(&pool, alice.id, page("")).await.unwrap();
        assert_eq!(
            muted_ids_of(&entries),
            vec![bob.id],
            "the page names the muted account"
        );
        assert_eq!(
            row_ids_of(&entries),
            vec![row.id],
            "and carries the mute row beside it"
        );
        assert_eq!(entries[0].0, row, "the row is the stored one");
        assert_eq!(entries[0].1.id, bob.id, "the user is the muted one");
        assert!(
            muting_page(&pool, bob.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "bob's own page holds nothing"
        );
    }

    /// Mastodon's defaults are the column defaults: a row written without
    /// either option hides notifications and never expires — the bare insert
    /// [`mute`] itself never makes.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_column_defaults_are_mastodon_s(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let sql = "INSERT INTO mutes (id, muter_id, muted_id) VALUES ($1, $2, $3) RETURNING *";
        let row = sqlx::query_as::<_, Mute>(sql)
            .bind(new_id())
            .bind(alice.id)
            .bind(bob.id)
            .fetch_one(&pool)
            .await
            .expect("the bare row inserts");

        assert!(
            row.hide_notifications,
            "the notifications default is true, so a mute hides them"
        );
        assert!(row.expires_at.is_none(), "and the duration default is none");
        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "and such a row is a mute like any other"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn notifications_false_mutes_the_timeline_only(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = write_mute(&pool, &alice, &bob, TIMELINE_ONLY).await;

        assert!(
            !row.hide_notifications,
            "the option is stored as the caller set it"
        );
        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "the pair is still muted"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and on the muted list"
        );
        assert!(
            notification_muted_ids(&pool, alice.id)
                .await
                .unwrap()
                .is_empty(),
            "but not on the notification-muted one"
        );
    }

    /// The timed side of the write: `expires_in` becomes an `expires_at` that
    /// many seconds ahead, and the reads keep answering the mute while that
    /// instant is still in the future.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_duration_stores_its_expiry(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let options = MuteOptions {
            notifications: true,
            expires_in: Some(3_600),
        };

        let row = write_mute(&pool, &alice, &bob, options).await;
        let ahead = seconds_ahead(&row);
        assert!(
            (3_590..=3_600).contains(&ahead),
            "the expiry is the requested hour from now, got {ahead}s: {:?}",
            row.expires_at
        );
        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "a mute whose expiry is in the future is a mute"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "on the list"
        );
        assert_eq!(
            notification_muted_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "and on the notification-muted one"
        );
    }

    /// `duration=0` is Mastodon's spelling of "no expiry", and a negative
    /// nonsense value is treated the same way rather than as an already expired
    /// mute.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_non_positive_duration_is_an_indefinite_mute(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;

        for expires_in in [Some(3_600), Some(0), Some(-60), None] {
            let options = MuteOptions {
                notifications: true,
                expires_in,
            };
            let row = mute(&pool, &alice, &bob, options)
                .await
                .expect("re-muting stores");
            let indefinite = expires_in.is_none_or(|secs| secs <= 0);
            assert_eq!(
                row.expires_at.is_none(),
                indefinite,
                "`expires_in` {expires_in:?} means {:?}",
                if indefinite { "indefinite" } else { "timed" }
            );
            assert!(
                is_muting(&pool, alice.id, bob.id).await.unwrap(),
                "and either way the pair is muted"
            );
        }
    }

    /// A mute whose `expires_at` lies in the past — what the row of a mute
    /// whose duration ran out looks like until the sweep deletes it — is absent
    /// from every read.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_expired_mute_is_absent_everywhere(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        mute_expiring_in(&pool, &alice, &bob, -60).await;
        mute_expiring_in(&pool, &alice, &carol, 60).await;

        assert!(
            !is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "the lapsed mute is not there to find"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![carol.id],
            "and its account is not on the muted list"
        );
        assert_eq!(
            notification_muted_ids(&pool, alice.id).await.unwrap(),
            vec![carol.id],
            "nor on the notification-muted one"
        );
        let entries = muting_page(&pool, alice.id, page("")).await.unwrap();
        assert_eq!(
            muted_ids_of(&entries),
            vec![carol.id],
            "and the page skips it"
        );
        assert_eq!(
            mute_count(&pool).await,
            2,
            "the row is still on the books until the sweep runs"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn muting_twice_updates_the_options_of_one_row(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = write_mute(&pool, &alice, &bob, MuteOptions::default()).await;

        let again = mute(&pool, &alice, &bob, TIMELINE_ONLY)
            .await
            .expect("the repeat mute is not an error");
        assert_eq!(again.id, first.id, "the pair keeps its one row");
        assert_eq!(
            again.inserted_at, first.inserted_at,
            "and its original insertion time"
        );
        assert!(
            !again.hide_notifications,
            "the options of the repeat write are stored"
        );
        assert_eq!(mute_count(&pool).await, 1, "nothing was written twice");

        let timed = MuteOptions {
            notifications: true,
            expires_in: Some(120),
        };
        let extended = mute(&pool, &alice, &bob, timed)
            .await
            .expect("re-muting with a duration stores");
        assert_eq!(extended.id, first.id, "still the same row");
        assert!(
            extended.hide_notifications,
            "and the latest options are the stored ones"
        );
        let ahead = seconds_ahead(&extended);
        assert!(
            (110..=120).contains(&ahead),
            "the duration is rewritten from the new write, got {ahead}s"
        );
    }

    /// Re-muting a pair whose mute has lapsed but is still stored revives it:
    /// the same row, a fresh expiry, and the reads find it again.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn re_muting_a_lapsed_mute_revives_the_row(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        mute_expiring_in(&pool, &alice, &bob, -60).await;
        let lapsed = stored_mute(&pool, &alice, &bob).await;
        assert!(
            !is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "the fixture mute is over"
        );

        let revived = mute(&pool, &alice, &bob, MuteOptions::default())
            .await
            .expect("muting again stores");
        assert_eq!(revived.id, lapsed.id, "the pair's row is reused");
        assert!(revived.expires_at.is_none(), "and is indefinite again");
        assert_eq!(mute_count(&pool).await, 1, "still one row for the pair");
        assert!(
            is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "and the reads find the mute again"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn unmuting_reports_the_row_it_removed(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;

        assert!(
            unmute(&pool, &alice, &bob).await.unwrap().is_none(),
            "a pair that was never muted has nothing to lift"
        );

        let row = write_mute(&pool, &alice, &bob, MuteOptions::default()).await;
        let removed = unmute(&pool, &alice, &bob).await.unwrap();
        assert_eq!(
            removed.as_ref(),
            Some(&row),
            "the removed row comes back as it stood"
        );
        assert!(
            !is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "and the fact is gone"
        );
        assert!(
            unmute(&pool, &alice, &bob).await.unwrap().is_none(),
            "lifting it twice finds nothing the second time"
        );
        assert_eq!(mute_count(&pool).await, 0, "and writes nothing");
    }

    /// A lapsed mute's row is nobody's mute to the reads and yet still a row
    /// for the pair key: lifting it is a delete of that row, and muting again
    /// afterwards is an ordinary write.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn unmute_removes_a_lapse_that_no_read_would_hide(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        mute_expiring_in(&pool, &alice, &bob, -60).await;

        let removed = unmute(&pool, &alice, &bob)
            .await
            .expect("lifting the lapsed mute runs");
        assert!(
            removed.is_some(),
            "the row the reads ignore is the row the delete reports"
        );
        assert_eq!(mute_count(&pool).await, 0, "and it is gone");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn muting_yourself_is_refused(pool: PgPool) {
        let alice = user(&pool, "alice").await;

        let err = mute(&pool, &alice, &alice, MuteOptions::default())
            .await
            .expect_err("nobody mutes themselves");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a self-mute is the 422 the endpoint renders: {err}"
        );
        assert_eq!(err.to_string(), "Can not mute yourself", "its wording");
        assert_eq!(mute_count(&pool).await, 0, "and it stored nothing");
        assert!(
            !is_muting(&pool, alice.id, alice.id).await.unwrap(),
            "the read agrees"
        );
    }

    /// The pair key the model's upsert builds on, checked against the table,
    /// and the directionality no constraint merges away.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_table_constraints_are_the_ones_the_model_relies_on(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        seed_mute(&pool, &alice, &bob, MuteOptions::default()).await;

        let duplicate =
            sqlx::query("INSERT INTO mutes (id, muter_id, muted_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(bob.id)
                .execute(&pool)
                .await
                .expect_err("one row per pair");
        assert_eq!(
            sqlstate_of(duplicate),
            "23505",
            "the pair is unique, which is what mute()'s upsert builds on"
        );

        // The same pair the other way is a different mute.
        let flipped = mute(&pool, &bob, &alice, TIMELINE_ONLY)
            .await
            .expect("the reverse pair is its own row");
        assert_eq!(mute_count(&pool).await, 2, "one row per direction");
        assert!(
            !flipped.hide_notifications,
            "each with its own stored options"
        );
    }

    /// Deleting either account takes the mute through the foreign key's
    /// cascade, from both ends of the pair.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_either_side_takes_the_mute(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        seed_mute(&pool, &alice, &bob, MuteOptions::default()).await;
        seed_mute(&pool, &carol, &alice, MuteOptions::default()).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the muted account deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "alice's mute of the deleted account went with it"
        );

        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the muting account deletes");
        assert_eq!(gone.rows_affected(), 1, "and so did the other account");
        assert_eq!(
            muted_ids(&pool, carol.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "carol's mute of the deleted account cascaded too"
        );
    }

    /// Mutes are each account's own: another account's mute of the same target
    /// is a separate row, and lifting one leaves the other.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn mutes_are_each_account_s_own(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        seed_mute(&pool, &alice, &carol, MuteOptions::default()).await;
        seed_mute(&pool, &bob, &carol, TIMELINE_ONLY).await;

        assert_eq!(
            muted_ids(&pool, carol.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "carol holds nothing of the mutes made against her"
        );

        let lifted = unmute(&pool, &alice, &carol)
            .await
            .expect("alice lifts her mute");
        assert!(lifted.is_some(), "and alice had a mute to lift");
        assert!(
            muted_ids(&pool, alice.id).await.unwrap().is_empty(),
            "alice's withdrawal left her own set empty"
        );
        assert_eq!(
            muted_ids(&pool, bob.id).await.unwrap(),
            vec![carol.id],
            "and bob's alone"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_page_of_mutes_newest_first_walks_by_row_id(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let erin = user(&pool, "erin").await;
        for nickname in ["bob", "carol", "dave"] {
            let muted = user(&pool, nickname).await;
            seed_mute(&pool, &alice, &muted, MuteOptions::default()).await;
        }
        let stored = stored_rows(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture mutes");

        let first = muting_page(&pool, alice.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(
            row_ids_of(&first),
            vec![stored[0].0, stored[1].0],
            "the two newest rows"
        );
        assert_eq!(
            muted_ids_of(&first),
            vec![stored[0].2, stored[1].2],
            "and their muted accounts, newest mute first"
        );

        let older = muting_page(&pool, alice.id, page(&format!("max_id={}", stored[1].0)))
            .await
            .expect("the continuation reads");
        assert_eq!(
            row_ids_of(&older),
            vec![stored[2].0],
            "max_id continues where the page ended"
        );

        let newer = muting_page(&pool, alice.id, page(&format!("min_id={}", stored[2].0)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            row_ids_of(&newer),
            vec![stored[0].0, stored[1].0],
            "a min_id page is answered newest-first too"
        );

        let tail = muting_page(&pool, alice.id, page(&format!("max_id={}", stored[2].0)))
            .await
            .expect("walking off the end is no error");
        assert!(tail.is_empty(), "and answers nothing");

        let other = muting_page(&pool, erin.id, page(""))
            .await
            .expect("a muter with nothing muted pages");
        assert!(other.is_empty(), "and holds nothing of alice's mutes");
    }

    /// The `mutes.expire` job's whole statement: expired rows go, indefinite
    /// ones and still-running timed ones stay.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_sweep_deletes_only_expired_rows(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        mute_expiring_in(&pool, &alice, &bob, -60).await;
        mute_expiring_in(&pool, &alice, &carol, -1).await;
        let indefinite = write_mute(&pool, &alice, &dave, MuteOptions::default()).await;
        let running = MuteOptions {
            notifications: true,
            expires_in: Some(600),
        };
        seed_mute(&pool, &bob, &dave, running).await;

        let removed = delete_expired(&pool).await.expect("the sweep runs");
        assert_eq!(removed, 2, "both lapsed rows went");
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![dave.id],
            "the indefinite mute is untouched"
        );
        assert_eq!(
            stored_mute(&pool, &alice, &dave).await.id,
            indefinite.id,
            "the same row, not a rewritten one"
        );
        assert!(
            is_muting(&pool, bob.id, dave.id).await.unwrap(),
            "and a mute whose expiry is still ahead stays"
        );

        let again = delete_expired(&pool).await.expect("a second sweep runs");
        assert_eq!(again, 0, "and finds nothing left to delete");
        assert_eq!(mute_count(&pool).await, 2, "leaving the live rows alone");
    }

    /// A sweep of an empty table is a no-op, so the periodic job costs nothing
    /// when there is nothing to do.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_sweep_of_an_empty_table_deletes_nothing(pool: PgPool) {
        let removed = delete_expired(&pool)
            .await
            .expect("the sweep over nothing runs");
        assert_eq!(removed, 0, "no rows, no deletions");
    }

    /// A mute of one second is a live mute the moment it is written and a
    /// lapsed one once its instant has passed: the reads and the sweep agree on
    /// which side of the database's `now()` it is on.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_lapsing_mute_leaves_the_reads_with_the_sweep(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let options = MuteOptions {
            notifications: true,
            expires_in: Some(1),
        };
        let row = write_mute(&pool, &alice, &bob, options).await;
        let ahead = seconds_ahead(&row);
        assert!(
            (0..=1).contains(&ahead),
            "the expiry is the second the caller asked for, got {ahead}s ahead \
             at {}",
            row.expires_at
                .map_or_else(|| "nothing".to_owned(), |at| mastodon_time(&at))
        );

        let lapsed = "UPDATE mutes SET expires_at = now() - make_interval(secs => 1)
                       WHERE muter_id = $1 AND muted_id = $2";
        let moved = sqlx::query(lapsed)
            .bind(alice.id)
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture moves the expiry into the past");
        assert_eq!(moved.rows_affected(), 1, "the row was there to move");

        assert!(
            !is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "the reads dropped it"
        );
        let removed = delete_expired(&pool).await.expect("the sweep runs");
        assert_eq!(removed, 1, "and so does the sweep");
        assert_eq!(mute_count(&pool).await, 0, "leaving nothing behind");
    }
}
