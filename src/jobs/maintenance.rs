//! Periodic maintenance jobs: the housekeeping sweeps that reclaim rows whose
//! useful life has ended.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module holds the recurring sweeps whose kinds live under the
//! `maintenance.` prefix. Each entry is one pass over one table, enqueued by
//! the [`periodic_jobs`](super::scheduler::periodic_jobs) schedule and run
//! through the [`JobRegistry`](super::JobRegistry) like every other kind.
//!
//! [`handle_notifications`] (job kind [`KIND_NOTIFICATIONS`]) is the first.
//! Dismissal is soft: the dismissal endpoints stamp
//! [`dismissed_at`](crate::models::notification::Notification::dismissed_at)
//! (`migrations/1794145737_notification_dismissed_at.sql`) and every read
//! filters those rows out, so they stay on the books forever once hidden.
//! The sweep deletes the ones dismissed longer than `RETENTION_DAYS` ago,
//! once per day, the window Pleroma-style retention gives them.
//!
//! [`handle_tokens`] (job kind [`KIND_TOKENS`]) is the second: one hourly
//! pass over the four short-lived credential tables, deleting the rows no
//! read accepts any more — `oauth_tokens` expired beyond the week their
//! refresh token buys them, spent or expired `oauth_authorizations` codes,
//! expired `mfa_tokens` challenges, and spent or lapsed
//! `password_reset_tokens` — summing the rows reclaimed into one count.
//! The reference is Pleroma's `ClearOauthTokenWorker`
//! (`lib/pleroma/workers/cron/clear_oauth_token_worker.ex`), which clears
//! expired tokens on a daily cron; here one sweep covers every credential
//! table, on the hour.
//!
//! [`handle_jobs`] (job kind [`KIND_JOBS`]) is the third: one daily pass of
//! batched prunes. It first deletes the `jobs` rows that reached a terminal
//! state more than `JOBS_RETENTION_DAYS` days ago — which sweeps the expired
//! incoming-activity deduplication rows out too, since this schema carries
//! that bookkeeping on the queue's `incoming:{activity id}` unique key (see
//! [`crate::api::activitypub`]) rather than in a table of its own. It then
//! prunes the `received_activities` rows older than
//! `RECEIVED_ACTIVITIES_RETENTION_DAYS` days, existence-guarded: a probe at
//! job time asks whether the table is there, and today it is not, so the step
//! logs a debug and skips — while a later ticket adding that table lights the
//! prune up with no change here. The reference is the cron purges under
//! Pleroma's `lib/pleroma/workers/cron/`.
//!
//! [`handle_purge_deleted_statuses`] (job kind [`KIND_DELETED_STATUSES`]) is
//! the fourth: one daily pass hard-deleting the LOCAL statuses soft-deleted
//! more than
//! [`Config::purge_deleted_statuses_days`](crate::config::Config::purge_deleted_statuses_days)
//! days ago, `0` disabling the sweep. Remote tombstones (`local = false`)
//! are never touched: their rows and unique `uri`s stay so a deleted remote
//! post is never re-imported (see the `deleted_at` column comment in
//! `migrations/1791712088_statuses.sql`). Dependents — `media_attachments`
//! by `status_id`, favourites, notifications — follow each deleted status
//! through their `ON DELETE CASCADE` foreign keys, not by hand. The
//! reference is Pleroma's status-expiry `Purge` step, here driven by the
//! configured age instead of per-row `expires_at`.
//!
//! [`handle_sweep_media`] (job kind [`KIND_MEDIA`]) is the fifth: a
//! six-hourly sweep of locally stored media no status ever claimed. The
//! candidates are `status_id IS NULL` (unposted), `remote_url IS NULL`
//! (this instance's own file, not a mirror), at least `MEDIA_STALE_HOURS`
//! hours old, and not referenced by any
//! `chat_messages` row — today the only other stored reference to unposted
//! media (`attachment_media_id`, `migrations/1791714489_chats.sql`). Each
//! reclaimed row loses its stored file first, through
//! [`UploaderBackend::delete`](crate::uploads::UploaderBackend::delete),
//! and then itself; a file whose removal fails keeps its row for the next
//! pass, so the sweep never deletes a pointer and strands its bytes. The
//! rows are walked in id-cursored batches of `MEDIA_BATCH_SIZE` so one
//! kept row cannot re-loop inside a run.

use serde_json::Value;
use sqlx::{PgPool, query, query_as, query_scalar};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of the dismissed-notification sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_notifications`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_NOTIFICATIONS: &str = "maintenance.notifications";

/// How long a dismissed notification stays in the table before the sweep
/// reclaims it: thirty days. Reads stopped answering it the moment it was
/// dismissed; this is only how long the row itself lives on.
const RETENTION_DAYS: i32 = 30;

/// The `DELETE` behind [`handle_notifications`]: every notification row
/// whose soft dismissal (`dismissed_at IS NOT NULL`) is older than
/// [`RETENTION_DAYS`] days.
const SWEEP_SQL: &str = "DELETE FROM notifications
        WHERE dismissed_at IS NOT NULL
          AND dismissed_at < now() - make_interval(days => $1)";

/// Runs one sweep and returns the number of rows it deleted.
///
/// Split out from [`handle_notifications`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](super::JobRegistry) expects.
async fn delete_dismissed(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(SWEEP_SQL).bind(RETENTION_DAYS).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_NOTIFICATIONS`]:
/// one sweep of the notifications dismissed longer than `RETENTION_DAYS`
/// days ago, logging how many rows it deleted.
///
/// A failed sweep is a [`JobError::Retry`], and the scheduler enqueues the
/// next period regardless, so the same sweep runs again shortly; the rows are
/// only invisible, never needed, so a pass lost to a database outage costs
/// nothing but the storage.
pub fn handle_notifications(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match delete_dismissed(&state.db).await {
            Ok(deleted) => {
                info!(deleted, "dismissed notifications deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "dismissed notification sweep failed: {err}"
            ))),
        }
    })
}

/// The job kind of the OAuth/MFA/password-reset token sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_tokens`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_TOKENS: &str = "maintenance.tokens";

/// How long an expired `oauth_tokens` row stays in the table before the
/// sweep reclaims it: seven days. The pair's refresh token remains a
/// usable credential for exactly this long past `valid_until` — the token
/// endpoint, not a retention job, decides when a refresh stops working —
/// so deleting earlier would break clients entitled to refresh.
const OAUTH_TOKEN_GRACE_DAYS: i32 = 7;

/// The `DELETE` behind [`delete_long_expired_oauth_tokens`]: every token
/// row whose `valid_until` is more than [`OAUTH_TOKEN_GRACE_DAYS`] days in
/// the past.
const DELETE_OAUTH_TOKENS_SQL: &str = "DELETE FROM oauth_tokens
         WHERE valid_until < now() - make_interval(days => $1)";

/// The `DELETE` behind [`delete_dead_authorizations`]: every code that has
/// been exchanged (`used = true`) or has reached `valid_until`. Codes are
/// one-time credentials with no grace: [`consume_authorization`] refuses
/// them at or past expiry, so such a row can never work again.
///
/// [`consume_authorization`]: crate::models::oauth_token::consume_authorization
const DELETE_AUTHORIZATIONS_SQL: &str = "DELETE FROM oauth_authorizations
         WHERE used = true OR valid_until <= now()";

/// The `DELETE` behind [`delete_expired_mfa_tokens`]: every challenge row
/// at or past `valid_until`, the boundary
/// [`mfa_token::get_valid`](crate::models::mfa_token::get_valid) already
/// refuses.
const DELETE_MFA_TOKENS_SQL: &str = "DELETE FROM mfa_tokens WHERE valid_until <= now()";

/// The `DELETE` behind [`delete_dead_password_reset_tokens`]: every reset
/// token the reset flow has stopped answering — spent, or issued at or
/// beyond the bound interval, the negation of
/// [`password_reset::get_valid`](crate::models::password_reset::get_valid)'s
/// `inserted_at > now() - $1::interval`. Rows inside their window stay.
const DELETE_PASSWORD_RESET_TOKENS_SQL: &str = "DELETE FROM password_reset_tokens
         WHERE used = true OR inserted_at <= now() - $1::interval";

/// Deletes the `oauth_tokens` expired beyond [`OAUTH_TOKEN_GRACE_DAYS`]
/// days, reporting how many rows went.
async fn delete_long_expired_oauth_tokens(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_OAUTH_TOKENS_SQL)
        .bind(OAUTH_TOKEN_GRACE_DAYS)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// Deletes the `oauth_authorizations` that are spent or expired, reporting
/// how many rows went.
async fn delete_dead_authorizations(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_AUTHORIZATIONS_SQL).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// Deletes the `mfa_tokens` at or past `valid_until`, reporting how many
/// rows went.
async fn delete_expired_mfa_tokens(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_MFA_TOKENS_SQL).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// Deletes the `password_reset_tokens` that are spent or outside the
/// validity window `ttl` — the window [`handle_tokens`] reads from
/// [`Config::password_reset_ttl_secs`](crate::config::Config::password_reset_ttl_secs)
/// and [`password_reset::get_valid`](crate::models::password_reset::get_valid)
/// applies on reads — reporting how many rows went.
async fn delete_dead_password_reset_tokens(
    pool: &PgPool,
    ttl: chrono::Duration,
) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_PASSWORD_RESET_TOKENS_SQL)
        .bind(ttl)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// Runs one token sweep: the four table deletes, their rows-affected
/// summed into one count.
///
/// Split out from [`handle_tokens`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](super::JobRegistry) expects.
async fn sweep_tokens(state: &AppState) -> Result<u64, sqlx::Error> {
    let ttl = chrono::Duration::try_seconds(
        i64::try_from(state.config.password_reset_ttl_secs).unwrap_or(i64::MAX),
    )
    .unwrap_or(chrono::Duration::MAX);
    let oauth_tokens = delete_long_expired_oauth_tokens(&state.db).await?;
    let authorizations = delete_dead_authorizations(&state.db).await?;
    let mfa_tokens = delete_expired_mfa_tokens(&state.db).await?;
    let reset_tokens = delete_dead_password_reset_tokens(&state.db, ttl).await?;
    Ok(oauth_tokens + authorizations + mfa_tokens + reset_tokens)
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_TOKENS`]: one
/// sweep of the dead credential rows, logging how many it deleted.
///
/// The four deletes cover `oauth_tokens` expired beyond the
/// `OAUTH_TOKEN_GRACE_DAYS`-day refresh grace, `oauth_authorizations`
/// spent or expired, `mfa_tokens` expired, and `password_reset_tokens`
/// spent or outside the configured ttl window. Every row they match is one
/// no read path accepts any more, so the sweep only reclaims storage.
///
/// A failed pass is a [`JobError::Retry`], and the scheduler enqueues the
/// next hour regardless, so the same sweep runs again shortly; a pass lost
/// to a database outage costs nothing but the storage.
pub fn handle_tokens(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match sweep_tokens(&state).await {
            Ok(deleted) => {
                info!(deleted, "expired tokens deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!("token sweep failed: {err}"))),
        }
    })
}

/// The job kind of the jobs-table prune, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_jobs`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_JOBS: &str = "maintenance.jobs";

/// How long a terminal (`completed` or `discarded`) job row stays in the
/// table after its last change before the prune reclaims it: seven days.
/// Nothing reads a terminal row after that; its `unique_key` (if it had one)
/// was freed the moment the row went terminal, so deleting it cannot wake a
/// deduplicated enqueue.
const JOBS_RETENTION_DAYS: i32 = 7;

/// How long a `received_activities` replay-guard row may stay before the
/// prune reclaims it: fourteen days. A redelivery after that window is
/// treated as a new activity, the same trade Pleroma-style retention makes.
const RECEIVED_ACTIVITIES_RETENTION_DAYS: i32 = 14;

/// Rows one round of the batched prunes claims. Walking a backlog in
/// [`PRUNE_BATCH_SIZE`]-row batches keeps every `DELETE` short enough that
/// one pass never locks a table across the whole sweep.
const PRUNE_BATCH_SIZE: i64 = 1_000;

/// The batched `DELETE` behind [`prune_jobs`]: up to [`PRUNE_BATCH_SIZE`]
/// terminal rows whose `updated_at` is older than the retention `$1`, chosen
/// by `id` so the batches are ordered and no row is ever visited twice.
const PRUNE_JOBS_SQL: &str = "DELETE FROM jobs
         WHERE id IN (
             SELECT id FROM jobs
              WHERE state IN ('completed', 'discarded')
                AND updated_at < now() - make_interval(days => $1)
              ORDER BY id
              LIMIT $2
         )";

/// The probe behind [`prune_received_activities`]: the table's name when the
/// schema has it, `NULL` when it does not. `to_regclass` answers from the
/// catalog without touching the table, so the statement is safe against a
/// schema that never gained one.
const RECEIVED_ACTIVITIES_EXISTS_SQL: &str =
    "SELECT to_regclass('public.received_activities')::text";

/// The batched `DELETE` behind [`prune_received_activities`]: the same shape
/// as [`PRUNE_JOBS_SQL`], against the replay-guard table's `id` and
/// `inserted_at`. It is only ever sent after the existence probe found the
/// table this pass.
const PRUNE_RECEIVED_ACTIVITIES_SQL: &str = "DELETE FROM received_activities
         WHERE id IN (
             SELECT id FROM received_activities
              WHERE inserted_at < now() - make_interval(days => $1)
              ORDER BY id
              LIMIT $2
         )";

/// Runs one batched prune: repeats `sql` — a `DELETE` shaped like
/// [`PRUNE_JOBS_SQL`], taking the retention days as `$1` and the batch size
/// as `$2` — until one round removes fewer rows than `batch`, summing the
/// rounds into the total rows reclaimed.
///
/// Shared by [`prune_jobs`] and [`prune_received_activities`]; a round that
/// comes back short (including zero on an empty or fresh table) ends the
/// loop, so one pass drains the whole backlog.
async fn prune_in_batches(
    pool: &PgPool,
    sql: &'static str,
    retention_days: i32,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    let batch_rows = u64::try_from(batch).unwrap_or(u64::MAX);
    let mut total = 0_u64;
    loop {
        let round = query(sql)
            .bind(retention_days)
            .bind(batch)
            .execute(pool)
            .await?;
        let deleted = round.rows_affected();
        total += deleted;
        if deleted < batch_rows {
            break;
        }
    }
    Ok(total)
}

/// Prunes the terminal `jobs` rows past [`JOBS_RETENTION_DAYS`], reporting
/// how many went. This is also where the expired incoming-activity
/// deduplication rows die: this schema has no `received_activities` table,
/// deduplication rides on the queue's `incoming:{activity id}` unique key
/// (see [`crate::api::activitypub`]), and a finished activity job is a
/// terminal row like any other.
async fn prune_jobs(pool: &PgPool) -> Result<u64, sqlx::Error> {
    prune_in_batches(pool, PRUNE_JOBS_SQL, JOBS_RETENTION_DAYS, PRUNE_BATCH_SIZE).await
}

/// Prunes the `received_activities` replay-guard rows past
/// [`RECEIVED_ACTIVITIES_RETENTION_DAYS`], guarded on the table existing.
///
/// The [`RECEIVED_ACTIVITIES_EXISTS_SQL`] probe runs at job time on every
/// pass, so nothing here assumes the table: today it does not exist, the
/// probe answers `NULL`, the step logs a debug and reports zero rows, and
/// once a later ticket adds the table the same pass deletes its stale rows.
async fn prune_received_activities(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let table = query_scalar::<_, Option<String>>(RECEIVED_ACTIVITIES_EXISTS_SQL)
        .fetch_one(pool)
        .await?;
    if table.is_none() {
        debug!("received_activities table absent; skipping the activity prune");
        return Ok(0);
    }
    prune_in_batches(
        pool,
        PRUNE_RECEIVED_ACTIVITIES_SQL,
        RECEIVED_ACTIVITIES_RETENTION_DAYS,
        PRUNE_BATCH_SIZE,
    )
    .await
}

/// Runs one prune pass: the terminal job rows, then the existence-guarded
/// `received_activities` rows, their counts summed into one total.
///
/// Split out from [`handle_jobs`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](super::JobRegistry) expects.
async fn sweep_prunes(state: &AppState) -> Result<u64, sqlx::Error> {
    let jobs = prune_jobs(&state.db).await?;
    let activities = prune_received_activities(&state.db).await?;
    Ok(jobs + activities)
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_JOBS`]: one
/// pass of the batched queue-row and replay-guard prunes, logging the total
/// number of rows it deleted.
///
/// Every row it reclaims is bookkeeping nothing reads any more — a job that
/// ended a week ago, a replay guard past its window — so the sweep only
/// frees storage. A failed pass is a [`JobError::Retry`], and the scheduler
/// enqueues the next day regardless, so a pass lost to a database outage
/// costs nothing but the storage.
pub fn handle_jobs(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match sweep_prunes(&state).await {
            Ok(deleted) => {
                info!(deleted, "pruned maintenance rows");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!("prune sweep failed: {err}"))),
        }
    })
}

/// The job kind of the deleted-local-status purge, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_purge_deleted_statuses`],
/// on [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_DELETED_STATUSES: &str = "maintenance.deleted_statuses";

/// The `DELETE` behind [`purge_deleted`]: every status row this instance
/// owns (`local = true`), that is soft-deleted (`deleted_at IS NOT NULL`),
/// and whose deletion is older than the configured `days` of age `$1`.
/// Remote tombstones (`local = false`) never match: the row and its unique
/// `uri` stay so a deleted remote post is never re-imported (see the
/// `deleted_at` column comment in `migrations/1791712088_statuses.sql`).
/// Dependents — `media_attachments` by `status_id`, favourites,
/// notifications — follow each deleted status through their
/// `ON DELETE CASCADE` foreign keys, never by hand here.
const PURGE_DELETED_STATUSES_SQL: &str = "DELETE FROM statuses
         WHERE deleted_at IS NOT NULL
           AND local = true
           AND deleted_at < now() - make_interval(days => $1)";

/// Runs one purge and returns the number of status rows it deleted.
///
/// Split out from [`handle_purge_deleted_statuses`] so the handler is only
/// the logging-and-error shape the [`JobRegistry`](super::JobRegistry)
/// expects.
async fn purge_deleted(pool: &PgPool, age_days: i32) -> Result<u64, sqlx::Error> {
    let deleted = query(PURGE_DELETED_STATUSES_SQL)
        .bind(age_days)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// The [`JobRegistry`](super::JobRegistry) handler for
/// [`KIND_DELETED_STATUSES`]: one daily hard purge of the stale deleted
/// local statuses, logging the number of rows it deleted.
///
/// The age comes from the configuration, not a literal: statuses
/// soft-deleted more than
/// [`Config::purge_deleted_statuses_days`](crate::config::Config::purge_deleted_statuses_days)
/// days ago go. A configured `0` disables the sweep: the pass deletes
/// nothing and reports a count of zero. Because the delete answers only to
/// `local = true` rows, remote tombstones survive every pass, and each
/// purged status's dependents go with it through their cascade
/// constraints.
///
/// A failed pass is a [`JobError::Retry`], and the scheduler enqueues the
/// next day regardless, so the same purge runs again shortly; a pass lost
/// to a database outage costs nothing but the storage.
pub fn handle_purge_deleted_statuses(
    state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async move {
        let age_days = i32::try_from(state.config.purge_deleted_statuses_days).unwrap_or(i32::MAX);
        if age_days == 0 {
            info!(deleted = 0, "deleted-status purge disabled");
            return Ok(());
        }
        match purge_deleted(&state.db, age_days).await {
            Ok(deleted) => {
                info!(deleted, "deleted local statuses purged");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "deleted-status purge failed: {err}"
            ))),
        }
    })
}

/// The job kind of the orphaned local-media sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_sweep_media`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_MEDIA: &str = "maintenance.media";

/// How long an unposted local media row must sit before the sweep may call
/// it abandoned: forty-eight hours. A client stores media first and names
/// it in the status only minutes later, so the grace covers that round
/// trip; past it nothing in the schema still names the file.
const MEDIA_STALE_HOURS: i32 = 48;

/// Rows one round of the orphaned-media walk claims. Walking the backlog
/// in [`MEDIA_BATCH_SIZE`]-row batches, ordered by `id` behind an id
/// cursor, keeps every statement short and — because the cursor only ever
/// moves past visited rows — guarantees a row kept after a failed file
/// delete is not revisited within the same run.
const MEDIA_BATCH_SIZE: i64 = 200;

/// The [`MediaCandidate`] fetch behind [`fetch_orphaned_batch`]: the next
/// [`MEDIA_BATCH_SIZE`] local, orphaned, stale rows past `id` cursor `$2`,
/// in id order.
///
/// "Local, orphaned, stale" is the ticket's conjunction: `status_id IS
/// NULL` (no status ever claimed the upload), `remote_url IS NULL` (a file
/// this instance stores, not a mirror of a remote one), and `inserted_at`
/// beyond [`MEDIA_STALE_HOURS`] hours ago. The `NOT EXISTS` arm skips the
/// media a live chat message still carries through
/// `attachment_media_id` — the one other stored reference to unposted
/// media in today's schema (`migrations/1791714489_chats.sql`).
///
/// The exclusion grows when the scheduled-statuses ticket lands: media
/// kept for a not-yet-due post will need the same guard against that
/// table. `scheduled_at` is only a deferred parameter in
/// `src/services/create_status.rs` today, so no table exists to name here
/// yet and this statement deliberately names none.
const SWEEP_MEDIA_CANDIDATES_SQL: &str = "SELECT id, storage_key FROM media_attachments
         WHERE status_id IS NULL
           AND remote_url IS NULL
           AND inserted_at < now() - make_interval(hours => $1)
           AND id > $2
           AND NOT EXISTS (
               SELECT 1 FROM chat_messages cm
                WHERE cm.attachment_media_id = media_attachments.id
           )
         ORDER BY id
         LIMIT $3";

/// The `DELETE` behind [`reclaim_media`]: one candidate row by id, issued
/// only once its stored file (if any) is gone.
const DELETE_MEDIA_ROW_SQL: &str = "DELETE FROM media_attachments WHERE id = $1";

/// One candidate row of the orphaned-media walk: enough to advance the
/// cursor, delete the file, and delete the row.
#[derive(Debug, sqlx::FromRow)]
struct MediaCandidate {
    /// The row's id: the walk's cursor and the delete's target.
    id: Uuid,
    /// The file's key inside the upload storage; `None` when the row has
    /// no stored file and needs no file step at all.
    storage_key: Option<String>,
}

/// Fetches the next batch of orphaned-media candidates past `after`, the
/// shape documented on [`SWEEP_MEDIA_CANDIDATES_SQL`].
async fn fetch_orphaned_batch(
    pool: &PgPool,
    after: Uuid,
) -> Result<Vec<MediaCandidate>, sqlx::Error> {
    query_as::<_, MediaCandidate>(SWEEP_MEDIA_CANDIDATES_SQL)
        .bind(MEDIA_STALE_HOURS)
        .bind(after)
        .bind(MEDIA_BATCH_SIZE)
        .fetch_all(pool)
        .await
}

/// Reclaims one candidate: the stored file goes first when there is one,
/// and only then the row; reports the rows it deleted (`1`) or kept (`0`).
///
/// A file the uploader refuses to remove — a key [`check_key`] rejects, or
/// bytes that are not on disk — keeps the row: the next pass tries again,
/// and storage never ends up with a file whose database pointer was
/// deleted around it.
///
/// [`check_key`]: crate::uploads::check_key
async fn reclaim_media(state: &AppState, row: MediaCandidate) -> Result<u64, sqlx::Error> {
    if let Some(key) = row.storage_key.as_deref()
        && let Err(err) = state.uploader.delete(key).await
    {
        warn!(
            %err,
            media_id = %row.id,
            "orphaned media file delete failed; keeping the row for the next sweep"
        );
        return Ok(0);
    }
    let removed = query(DELETE_MEDIA_ROW_SQL)
        .bind(row.id)
        .execute(&state.db)
        .await?;
    Ok(removed.rows_affected())
}

/// Runs one sweep: the whole stale-orphan backlog, walked in
/// [`MEDIA_BATCH_SIZE`]-row batches behind a forward-only id cursor, the
/// rows reclaimed summed into one count.
///
/// The cursor starts at the nil uuid — below every `UUIDv7` id (decision 4)
/// — and lands on the last row of each batch, kept or deleted alike, so a
/// row that survived its failed file delete waits for the next run instead
/// of re-appearing in this one. A short batch ends the walk.
///
/// Split out from [`handle_sweep_media`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](super::JobRegistry) expects.
async fn sweep_media(state: &AppState) -> Result<u64, sqlx::Error> {
    let batch_limit = usize::try_from(MEDIA_BATCH_SIZE).unwrap_or(usize::MAX);
    let mut cursor = Uuid::nil();
    let mut deleted = 0_u64;
    loop {
        let batch = fetch_orphaned_batch(&state.db, cursor).await?;
        let visited = batch.len();
        if let Some(last) = batch.last() {
            cursor = last.id;
        }
        for candidate in batch {
            deleted += reclaim_media(state, candidate).await?;
        }
        if visited < batch_limit {
            break;
        }
    }
    Ok(deleted)
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_MEDIA`]: one
/// sweep of the stale, orphaned local media rows, logging the number of
/// rows it deleted.
///
/// A file that cannot be removed is not a failed pass: its row is kept,
/// warned about, and retried by the next run, so one unreadable file never
/// strands the sweep on the rest. Only a database failure is a
/// [`JobError::Retry`], and the scheduler enqueues the next six hours
/// regardless; a pass lost to an outage costs nothing but the storage.
pub fn handle_sweep_media(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match sweep_media(&state).await {
            Ok(deleted) => {
                info!(deleted, "orphaned local media deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "orphaned media sweep failed: {err}"
            ))),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use bytes::Bytes;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        jobs::{QUEUE_BACKGROUND, scheduler::periodic_jobs},
        models::{
            job::{NewJob, enqueue},
            oauth_app::{NewOauthApp, OauthApp, create as create_app},
            user::{NewUser, User, insert, local_ap_id},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// The period the notification sweep is scheduled on: one day.
    const EVERY: Duration = Duration::from_hours(24);

    /// The period the token sweep is scheduled on: one hour.
    const HOURLY: Duration = Duration::from_secs(3_600);

    /// The period the orphaned-media sweep is scheduled on: six hours.
    const SIX_HOURS: Duration = Duration::from_secs(21_600);

    /// One day in the seconds the fixture helpers count ages in.
    const DAY: i64 = 86_400;

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        state_with_config(pool, Config::for_tests())
    }

    /// Test state over `pool` with an explicit `config`, for handlers whose
    /// work reads the configuration — the token sweep's reset window.
    fn state_with_config(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Test state over a pool that cannot answer: closed before it ever
    /// connected, which is how a handler sees a database that went away.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
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

    /// Inserts a notification for `recipient` caused by `actor` whose
    /// dismissal is stamped `secs` seconds in the past, so a test can put the
    /// table into the state the sweep is about without waiting it out.
    async fn dismissed_ago(pool: &PgPool, recipient: &User, actor: &User, secs: i64) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO notifications (id, user_id, from_account_id, type, dismissed_at)
                    VALUES ($1, $2, $3, 'mention', now() - make_interval(secs => $4))";
        let written = query(sql)
            .bind(id)
            .bind(recipient.id)
            .bind(actor.id)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the fixture notification inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a notification for `recipient` caused by `actor` that was
    /// never dismissed, returning its id.
    async fn undismissed(pool: &PgPool, recipient: &User, actor: &User) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO notifications (id, user_id, from_account_id, type, dismissed_at)
                    VALUES ($1, $2, $3, 'mention', NULL)";
        let written = query(sql)
            .bind(id)
            .bind(recipient.id)
            .bind(actor.id)
            .execute(pool)
            .await
            .expect("the fixture notification inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Registers an OAuth app named `name`: the issuer the credential rows
    /// point at through their `app_id` foreign keys.
    async fn register_app(pool: &PgPool, name: &str) -> OauthApp {
        create_app(
            pool,
            &NewOauthApp {
                client_name: name.to_owned(),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: "read".to_owned(),
                website: None,
                user_id: None,
            },
        )
        .await
        .expect("the app fixture inserts")
    }

    /// Inserts an `oauth_tokens` row for `owner` issued to `client` whose
    /// `valid_until` is `secs` seconds from now (negative for the past), so
    /// a test can set the table into any point of the grace window.
    async fn oauth_token_expiring_in(
        pool: &PgPool,
        owner: &User,
        client: &OauthApp,
        secs: i64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO oauth_tokens
                       (id, token, refresh_token, user_id, app_id, scopes, valid_until)
                   VALUES ($1, $2, $3, $4, $5, 'read', now() + make_interval(secs => $6))";
        let written = query(sql)
            .bind(id)
            .bind(format!("access-{id}"))
            .bind(format!("refresh-{id}"))
            .bind(owner.id)
            .bind(client.id)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the fixture oauth token inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts an `oauth_authorizations` code for `owner` issued to
    /// `client`, expiring `secs` seconds from now and already exchanged
    /// when `used`, returning its id.
    async fn authorization_row(
        pool: &PgPool,
        owner: &User,
        client: &OauthApp,
        secs: i64,
        used: bool,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO oauth_authorizations
                       (id, token, user_id, app_id, scopes, valid_until, used)
                   VALUES ($1, $2, $3, $4, 'read', now() + make_interval(secs => $5), $6)";
        let written = query(sql)
            .bind(id)
            .bind(format!("code-{id}"))
            .bind(owner.id)
            .bind(client.id)
            .bind(secs)
            .bind(used)
            .execute(pool)
            .await
            .expect("the fixture authorization inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts an `mfa_tokens` challenge for `owner` whose `valid_until` is
    /// `secs` seconds from now, returning its id.
    async fn mfa_token_expiring_in(pool: &PgPool, owner: &User, secs: i64) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO mfa_tokens (id, token, user_id, authorization_id, valid_until)
                   VALUES ($1, $2, $3, NULL, now() + make_interval(secs => $4))";
        let written = query(sql)
            .bind(id)
            .bind(format!("mfa-{id}"))
            .bind(owner.id)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the fixture mfa challenge inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a `password_reset_tokens` row for `owner` issued `age_secs`
    /// in the past and spent when `used`, returning its id.
    async fn password_reset_row(pool: &PgPool, owner: &User, age_secs: i64, used: bool) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO password_reset_tokens (id, token, user_id, used, inserted_at)
                   VALUES ($1, $2, $3, $4, now() - make_interval(secs => $5))";
        let written = query(sql)
            .bind(id)
            .bind(format!("reset-{id}"))
            .bind(owner.id)
            .bind(used)
            .bind(age_secs)
            .execute(pool)
            .await
            .expect("the fixture reset token inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Whether `table` — one of the swept tables, named by the test — still
    /// holds the row with primary key `id`. The table names are test
    /// constants, never user input, so the dynamic statement is asserted
    /// safe (the same bypass `src/test_db.rs` documents).
    async fn row_exists(pool: &PgPool, table: &str, id: Uuid) -> bool {
        let rows = query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE id = $1"
        )))
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the row count reads");
        rows == 1
    }

    /// The ids of every notification row left in the table, ordered, so a
    /// test can see exactly which rows survived the sweep.
    async fn surviving(pool: &PgPool) -> Vec<Uuid> {
        query_scalar::<_, Uuid>("SELECT id FROM notifications ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("the rows read")
    }

    /// The handler's attempt info: the first of however many the sweep gets,
    /// since its work does not depend on which pass it is.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn the_handler_deletes_only_the_stale_dismissed_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let stale = dismissed_ago(&pool, &alice, &bob, 31 * DAY).await;
        let recent = dismissed_ago(&pool, &alice, &bob, DAY).await;
        let live = undismissed(&pool, &alice, &bob).await;

        let outcome = handle_notifications(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep of stale rows succeeds");
        let survivors = surviving(&pool).await;
        assert!(
            !survivors.contains(&stale),
            "the row dismissed beyond the retention window is gone"
        );
        let mut expected = vec![recent, live];
        expected.sort();
        assert_eq!(
            survivors, expected,
            "only the stale row is gone: the recently dismissed one and \
             the undismissed one stay"
        );
    }

    #[tokio::test]
    async fn the_handler_passes_over_a_table_it_need_not_touch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let outcome = handle_notifications(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
        assert!(surviving(&pool).await.is_empty(), "and it wrote nothing");
    }

    /// A sweep that cannot reach the database is a retryable failure, so the
    /// queue keeps the pass while the scheduler enqueues the next period: the
    /// stale rows wait for a working sweep instead of being assumed gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = broken_state().await;

        let outcome = handle_notifications(state, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("dismissed notification sweep failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            surviving(&pool).await.is_empty(),
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// The sweep is wired into both registries the ticket names: the worker
    /// can run its kind, and the scheduler places it on the maintenance queue
    /// once a day.
    #[test]
    fn the_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_NOTIFICATIONS).is_some(),
            "the worker knows `{KIND_NOTIFICATIONS}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_NOTIFICATIONS)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, EVERY, "once a day");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_notifications`], not some other sweep: calling the registered
    /// handler deletes the rows.
    #[tokio::test]
    async fn the_registered_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let stale = dismissed_ago(&pool, &alice, &bob, 31 * DAY).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_NOTIFICATIONS)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        let survivors = surviving(&pool).await;
        assert!(
            !survivors.contains(&stale),
            "and it is this module's sweep: the stale row is gone"
        );
        assert!(survivors.is_empty(), "and nothing was left behind");
    }

    /// The token sweep deletes exactly the dead rows of each table and
    /// keeps everything still inside its validity window.
    #[tokio::test]
    async fn the_token_sweep_deletes_only_the_dead_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let client = register_app(&pool, "Client").await;
        let expired_token = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
        let graced_token = oauth_token_expiring_in(&pool, &alice, &client, -6 * DAY).await;
        let expired_code = authorization_row(&pool, &alice, &client, -60, false).await;
        let spent_code = authorization_row(&pool, &alice, &client, 600, true).await;
        let live_code = authorization_row(&pool, &alice, &client, 600, false).await;
        let stale_challenge = mfa_token_expiring_in(&pool, &alice, -1).await;
        let fresh_challenge = mfa_token_expiring_in(&pool, &alice, 300).await;
        let spent_reset = password_reset_row(&pool, &alice, 60, true).await;
        let lapsed_reset = password_reset_row(&pool, &alice, DAY + 60, false).await;
        let fresh_reset = password_reset_row(&pool, &alice, 60, false).await;

        let outcome = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep over dead and live rows succeeds");
        assert!(
            !row_exists(&pool, "oauth_tokens", expired_token).await,
            "a token expired eight days ago is past its refresh-token week"
        );
        assert!(
            row_exists(&pool, "oauth_tokens", graced_token).await,
            "a token expired six days ago can still be refreshed and stays"
        );
        assert!(
            !row_exists(&pool, "oauth_authorizations", expired_code).await,
            "an expired code goes even unused: it can never be redeemed"
        );
        assert!(
            !row_exists(&pool, "oauth_authorizations", spent_code).await,
            "a spent code goes even fresh: it has been redeemed once already"
        );
        assert!(
            row_exists(&pool, "oauth_authorizations", live_code).await,
            "a fresh unused code stays"
        );
        assert!(
            !row_exists(&pool, "mfa_tokens", stale_challenge).await,
            "an expired challenge goes"
        );
        assert!(
            row_exists(&pool, "mfa_tokens", fresh_challenge).await,
            "a pending challenge stays"
        );
        assert!(
            !row_exists(&pool, "password_reset_tokens", spent_reset).await,
            "a spent reset token goes even fresh"
        );
        assert!(
            !row_exists(&pool, "password_reset_tokens", lapsed_reset).await,
            "a reset token older than the default one-day window goes"
        );
        assert!(
            row_exists(&pool, "password_reset_tokens", fresh_reset).await,
            "a fresh unused reset token stays"
        );
    }

    /// The reset window comes from the configuration, not from a literal:
    /// the same row survives the default one-day ttl and dies under an
    /// overridden one-hour one.
    #[tokio::test]
    async fn the_reset_window_comes_from_the_config() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let aged = password_reset_row(&pool, &alice, 3_700, false).await;

        let default_pass = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;
        assert_eq!(default_pass, Ok(()), "the default-config sweep succeeds");
        assert!(
            row_exists(&pool, "password_reset_tokens", aged).await,
            "an hour-old token sits inside the default one-day window"
        );

        let mut config = Config::for_tests();
        config.password_reset_ttl_secs = 3_600;
        let short_window_pass =
            handle_tokens(state_with_config(&pool, config), FIRST_ATTEMPT, json!({})).await;
        assert_eq!(
            short_window_pass,
            Ok(()),
            "the overridden-ttl sweep succeeds"
        );
        assert!(
            !row_exists(&pool, "password_reset_tokens", aged).await,
            "the same token is outside a one-hour window and goes"
        );
    }

    #[tokio::test]
    async fn the_token_sweep_passes_over_tables_it_need_not_touch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
    }

    /// A sweep that cannot reach the database is a retryable failure, so
    /// the queue keeps the pass while the scheduler enqueues the next hour:
    /// the dead rows wait for a working sweep instead of being assumed gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_token_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let client = register_app(&pool, "Client").await;
        let expired = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
        let state = broken_state().await;

        let outcome = handle_tokens(state, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("token sweep failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            row_exists(&pool, "oauth_tokens", expired).await,
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// The token sweep is wired into both registries the ticket names: the
    /// worker can run its kind, and the scheduler places it on the
    /// maintenance queue once an hour.
    #[test]
    fn the_token_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_TOKENS).is_some(),
            "the worker knows `{KIND_TOKENS}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_TOKENS)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, HOURLY, "once an hour");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_tokens`], not some other sweep: calling the registered
    /// handler deletes the dead rows and keeps the live ones.
    #[tokio::test]
    async fn the_registered_token_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let client = register_app(&pool, "Client").await;
        let expired = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
        let live = authorization_row(&pool, &alice, &client, 600, false).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_TOKENS)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        assert!(
            !row_exists(&pool, "oauth_tokens", expired).await,
            "and it is this module's sweep: the expired token is gone"
        );
        assert!(
            row_exists(&pool, "oauth_authorizations", live).await,
            "and only the dead rows went: the live code stays"
        );
    }

    /// The `kind` the multi-batch fixture stamps its rows, so the test can
    /// count exactly its own rows back out of the table.
    const BULK_KIND: &str = "prune.bulk";

    /// Enqueues a job on the background queue through the very
    /// [`enqueue`] the producers use, then ages it: `state` and an
    /// `updated_at` `days` in the past, written with the `UPDATE` the
    /// worker's own transitions would have stamped. Returns the row id.
    async fn aged_job(pool: &PgPool, kind: &str, state: &str, days: i32) -> Uuid {
        let job = enqueue(
            pool,
            NewJob {
                queue: QUEUE_BACKGROUND,
                kind,
                args: json!({}),
                run_at: None,
                max_attempts: None,
                unique_key: None,
            },
        )
        .await
        .expect("the fixture job enqueues")
        .expect("an unkeyed enqueue is never deduplicated away");
        let sql = "UPDATE jobs SET state = $2, updated_at = now() - make_interval(days => $3)
                   WHERE id = $1";
        let written = query(sql)
            .bind(job.id)
            .bind(state)
            .bind(days)
            .execute(pool)
            .await
            .expect("the fixture ages the job");
        assert_eq!(written.rows_affected(), 1, "one row was rewritten");
        job.id
    }

    /// Seeds `count` stale terminal rows: [`aged_job`]'s end state, reached
    /// in two set-based statements because a multi-batch backlog is too
    /// large to name one enqueue at a time.
    async fn seed_many_stale(pool: &PgPool, count: i64) {
        let inserted = query(
            "INSERT INTO jobs (id, queue, kind, args, state, updated_at)
             SELECT gen_random_uuid(), 'background', $1, '{}'::jsonb, 'available', now()
               FROM generate_series(1, $2)",
        )
        .bind(BULK_KIND)
        .bind(count)
        .execute(pool)
        .await
        .expect("the bulk fixture inserts");
        assert_eq!(
            inserted.rows_affected(),
            u64::try_from(count).expect("the fixture seeds a positive count"),
            "every seeded row landed in the table"
        );
        let aged = query(
            "UPDATE jobs SET state = 'completed', updated_at = now() - make_interval(days => 8)
             WHERE kind = $1",
        )
        .bind(BULK_KIND)
        .execute(pool)
        .await
        .expect("the bulk fixture ages its rows");
        assert_eq!(
            aged.rows_affected(),
            u64::try_from(count).expect("the fixture ages a positive count"),
            "every seeded row turned stale and terminal"
        );
    }

    /// How many of the bulk-seeded rows are still in the table.
    async fn surviving_bulk_rows(pool: &PgPool) -> i64 {
        query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
            .bind(BULK_KIND)
            .fetch_one(pool)
            .await
            .expect("the rows read")
    }

    /// The jobs prune deletes exactly the stale terminal rows: a `completed`
    /// or `discarded` row past the week goes, a fresh terminal row and every
    /// live-state row — however old — stay.
    #[tokio::test]
    async fn the_jobs_prune_deletes_only_stale_terminal_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stale_completed = aged_job(&pool, "done.stale", "completed", 8).await;
        let stale_discarded = aged_job(&pool, "dead.stale", "discarded", 8).await;
        let fresh_completed = aged_job(&pool, "done.fresh", "completed", 1).await;
        let stale_available = aged_job(&pool, "wait.stale", "available", 8).await;
        let stale_retryable = aged_job(&pool, "retry.stale", "retryable", 8).await;
        let stale_executing = aged_job(&pool, "run.stale", "executing", 8).await;

        let outcome = handle_jobs(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a prune over stale and live rows succeeds");
        assert!(
            !row_exists(&pool, "jobs", stale_completed).await,
            "a completed row past the week is gone"
        );
        assert!(
            !row_exists(&pool, "jobs", stale_discarded).await,
            "a discarded row past the week is gone"
        );
        assert!(
            row_exists(&pool, "jobs", fresh_completed).await,
            "a completed row inside the week stays"
        );
        assert!(
            row_exists(&pool, "jobs", stale_available).await,
            "a stale available row is live work, not garbage"
        );
        assert!(
            row_exists(&pool, "jobs", stale_retryable).await,
            "a stale retryable row stays claimable"
        );
        assert!(
            row_exists(&pool, "jobs", stale_executing).await,
            "a stale executing row may still have a worker on it"
        );
    }

    /// A backlog larger than one batch is cleared by a single handler run:
    /// the loop keeps deleting until a round comes up short, it is not one
    /// batch per run.
    #[tokio::test]
    async fn the_jobs_prune_loops_until_the_backlog_is_gone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let backlog = PRUNE_BATCH_SIZE + 5;
        seed_many_stale(&pool, backlog).await;

        let outcome = handle_jobs(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the multi-batch pass succeeds");
        assert_eq!(
            surviving_bulk_rows(&pool).await,
            0,
            "one handler run cleared more than {PRUNE_BATCH_SIZE} rows: \
             the loop ran, not a single batch"
        );
    }

    /// With today's schema the replay-guard table does not exist, and that
    /// is a skip, not a failure: the pass completes and still prunes the
    /// jobs table, nothing else.
    #[tokio::test]
    async fn the_jobs_prune_skips_the_absent_received_activities_table() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let table = query_scalar::<_, Option<String>>(RECEIVED_ACTIVITIES_EXISTS_SQL)
            .fetch_one(&pool)
            .await
            .expect("the probe reads");
        assert!(
            table.is_none(),
            "today's schema has no received_activities table: the premise this test guards"
        );
        let stale = aged_job(&pool, "done.stale", "completed", 8).await;
        let live = aged_job(&pool, "wait.live", "available", 8).await;

        let outcome = handle_jobs(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(
            outcome,
            Ok(()),
            "the absent table is a debug skip, not an error"
        );
        assert!(
            !row_exists(&pool, "jobs", stale).await,
            "and the pass still prunes the jobs table"
        );
        assert!(
            row_exists(&pool, "jobs", live).await,
            "pruning only jobs leaves the live row"
        );
    }

    /// The jobs prune is wired into both registries the ticket names: the
    /// worker can run its kind, and the scheduler places it on the
    /// maintenance queue once a day.
    #[test]
    fn the_jobs_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_JOBS).is_some(),
            "the worker knows `{KIND_JOBS}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_JOBS)
            .expect("the prune is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, EVERY, "once a day");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_jobs`], not some other sweep: calling the registered
    /// handler deletes the stale terminal rows and keeps the fresh ones.
    #[tokio::test]
    async fn the_registered_jobs_handler_is_this_prune() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stale = aged_job(&pool, "done.stale", "completed", 8).await;
        let live = aged_job(&pool, "done.live", "completed", 1).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_JOBS)
            .expect("the prune's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        assert!(
            !row_exists(&pool, "jobs", stale).await,
            "and it is this module's prune: the stale terminal row is gone"
        );
        assert!(
            row_exists(&pool, "jobs", live).await,
            "and only the stale rows went: the fresh terminal row stays"
        );
    }

    /// A prune that cannot reach the database is a retryable failure, so
    /// the queue keeps the pass while the scheduler enqueues the next day:
    /// the stale rows wait for a working prune instead of being assumed
    /// gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_jobs_prune() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stale = aged_job(&pool, "done.stale", "completed", 8).await;
        let broken = broken_state().await;

        let outcome = handle_jobs(broken, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("prune sweep failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful prune"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            row_exists(&pool, "jobs", stale).await,
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// Inserts a live status — never deleted, owned by this instance when
    /// `local` — authored by `author`, and returns its id. Written
    /// directly, the way the deletion fixtures are: the purge reads only
    /// `deleted_at` and `local`, and the dependent cascade only needs the
    /// row to exist.
    async fn live_status_row(pool: &PgPool, author: &User, local: bool) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO statuses
                       (id, uri, url, user_id, visibility, conversation_id, local)
                   VALUES ($1, $2, $3, $4, 'public', $1, $5)";
        let written = query(sql)
            .bind(id)
            .bind(format!("{TEST_BASE_URL}/objects/{id}"))
            .bind(format!("{TEST_BASE_URL}/notice/{id}"))
            .bind(author.id)
            .bind(local)
            .execute(pool)
            .await
            .expect("the live status fixture inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a status — owned by this instance when `local` — authored
    /// by `author` whose soft deletion was stamped `secs` seconds in the
    /// past, and returns its id. Same direct write as [`live_status_row`].
    async fn deleted_status_row(pool: &PgPool, author: &User, local: bool, secs: i64) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO statuses
                       (id, uri, url, user_id, visibility, conversation_id, local, deleted_at)
                   VALUES ($1, $2, $3, $4, 'public', $1, $5,
                           now() - make_interval(secs => $6))";
        let written = query(sql)
            .bind(id)
            .bind(format!("{TEST_BASE_URL}/objects/{id}"))
            .bind(format!("{TEST_BASE_URL}/notice/{id}"))
            .bind(author.id)
            .bind(local)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the deleted status fixture inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a `media_attachments` row owned by `owner` attached to
    /// `status_id`, and returns its id — the dependent the status purge
    /// must take down with the status through the `status_id` cascade.
    async fn media_row(pool: &PgPool, owner: &User, status_id: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO media_attachments (id, user_id, status_id, url, content_type)
                    VALUES ($1, $2, $3, $4, 'image/png')";
        let written = query(sql)
            .bind(id)
            .bind(owner.id)
            .bind(status_id)
            .bind(format!("{TEST_BASE_URL}/media/{id}.png"))
            .execute(pool)
            .await
            .expect("the media fixture inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// The status purge deletes exactly the stale soft-deleted LOCAL rows:
    /// a local deletion inside the window, a live local post and — the
    /// tombstone rule — a stale deleted REMOTE row all stay.
    #[tokio::test]
    async fn the_status_purge_deletes_only_stale_deleted_local_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stale_local = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
        let stale_live = live_status_row(&pool, &alice, true).await;
        let stale_remote = deleted_status_row(&pool, &alice, false, 31 * DAY).await;
        let fresh_local = deleted_status_row(&pool, &alice, true, DAY).await;

        let outcome =
            handle_purge_deleted_statuses(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a purge over stale and kept rows succeeds");
        assert!(
            !row_exists(&pool, "statuses", stale_local).await,
            "the local tombstone past the default 30-day window is gone"
        );
        assert!(
            row_exists(&pool, "statuses", stale_live).await,
            "a live local row is nobody's garbage, however old"
        );
        assert!(
            row_exists(&pool, "statuses", stale_remote).await,
            "the remote tombstone stays: its row keeps the deleted post from being re-imported"
        );
        assert!(
            row_exists(&pool, "statuses", fresh_local).await,
            "a local deletion inside the window stays"
        );
    }

    /// The dependents ride the cascade: a stale deleted local status
    /// carrying a media attachment loses both, while the sweep itself
    /// names only the `statuses` table.
    #[tokio::test]
    async fn the_status_purge_takes_dependent_rows_with_the_cascade() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stale = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
        let attachment = media_row(&pool, &alice, stale).await;

        let outcome =
            handle_purge_deleted_statuses(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the cascade pass succeeds");
        assert!(
            !row_exists(&pool, "statuses", stale).await,
            "the stale local tombstone is gone"
        );
        assert!(
            !row_exists(&pool, "media_attachments", attachment).await,
            "and its attachment followed the status_id cascade"
        );
    }

    /// `purge_deleted_statuses_days = 0` on the config disables the sweep:
    /// the stale local tombstone every other pass above reclaims stays
    /// put, and the pass still ends clean.
    #[tokio::test]
    async fn zero_days_disables_the_status_purge() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stale_local = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
        let stale_remote = deleted_status_row(&pool, &alice, false, 31 * DAY).await;
        let mut config = Config::for_tests();
        config.purge_deleted_statuses_days = 0;

        let outcome = handle_purge_deleted_statuses(
            state_with_config(&pool, config),
            FIRST_ATTEMPT,
            json!({}),
        )
        .await;

        assert_eq!(outcome, Ok(()), "the disabled pass is a clean no-op");
        assert!(
            row_exists(&pool, "statuses", stale_local).await,
            "the stale local tombstone survives while the purge is off"
        );
        assert!(
            row_exists(&pool, "statuses", stale_remote).await,
            "and the remote tombstone was never in question anyway"
        );
    }

    /// The purge's window comes from the configuration, not a literal: a
    /// 45-day-old local tombstone sits inside an overridden 60-day window
    /// the default 30-day one would reclaim.
    #[tokio::test]
    async fn the_status_purge_window_comes_from_the_config() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let aged = deleted_status_row(&pool, &alice, true, 45 * DAY).await;
        let mut config = Config::for_tests();
        config.purge_deleted_statuses_days = 60;

        let outcome = handle_purge_deleted_statuses(
            state_with_config(&pool, config),
            FIRST_ATTEMPT,
            json!({}),
        )
        .await;

        assert_eq!(outcome, Ok(()), "the widened-window pass succeeds");
        assert!(
            row_exists(&pool, "statuses", aged).await,
            "a 45-day tombstone sits inside a 60-day window and stays"
        );
    }

    /// The status purge is wired into both registries the ticket names:
    /// the worker can run its kind, and the scheduler places it on the
    /// maintenance queue once a day.
    #[test]
    fn the_deleted_statuses_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_DELETED_STATUSES).is_some(),
            "the worker knows `{KIND_DELETED_STATUSES}`"
        );
        assert_eq!(
            KIND_DELETED_STATUSES, "maintenance.deleted_statuses",
            "the ticket's kind name"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_DELETED_STATUSES)
            .expect("the purge is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, EVERY, "once a day");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_purge_deleted_statuses`], not some other sweep: calling
    /// the registered handler reclaims the stale local tombstone and keeps
    /// the remote one.
    #[tokio::test]
    async fn the_registered_status_handler_is_this_purge() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stale_local = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
        let stale_remote = deleted_status_row(&pool, &alice, false, 31 * DAY).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_DELETED_STATUSES)
            .expect("the purge's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        assert!(
            !row_exists(&pool, "statuses", stale_local).await,
            "and it is this module's purge: the stale local tombstone is gone"
        );
        assert!(
            row_exists(&pool, "statuses", stale_remote).await,
            "and only the local tombstones went: the remote one stays"
        );
    }

    /// A purge that cannot reach the database is a retryable failure, so
    /// the queue keeps the pass while the scheduler enqueues the next day:
    /// the stale tombstones wait for a working purge instead of being
    /// assumed gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_status_purge() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stale = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
        let broken = broken_state().await;

        let outcome = handle_purge_deleted_statuses(broken, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("deleted-status purge failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful purge"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            row_exists(&pool, "statuses", stale).await,
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// Test state over `pool` whose uploads live in a fresh temporary
    /// directory, together with that directory's guard and the backend a
    /// test puts files through: the handler deletes from that same
    /// directory, so the guard must outlive both (the pattern of
    /// `src/uploads.rs`'s `backend_delegates_every_operation` test).
    fn uploads_state(pool: &PgPool) -> (tempfile::TempDir, AppState, UploaderBackend) {
        let dir = tempfile::TempDir::new().expect("a temp uploads dir");
        let mut config = Config::for_tests();
        config.uploads_dir = dir.path().to_path_buf();
        let backend = UploaderBackend::from(&config);
        let state = state_with_config(pool, config);
        (dir, state, backend)
    }

    /// Inserts a `media_attachments` row aged `hours_old` into the past,
    /// the sweep's inputs set as given: an unposted local upload is
    /// `status_id` NULL and `remote_url` NULL with a `storage_key`; a
    /// mirrored remote row is one with `remote_url` set. Returns its id.
    async fn unposted_media_row(
        pool: &PgPool,
        owner: &User,
        storage_key: Option<&str>,
        remote_url: Option<&str>,
        hours_old: i32,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO media_attachments
                       (id, user_id, status_id, storage_key, remote_url, url, content_type,
                        inserted_at)
                   VALUES ($1, $2, NULL, $3, $4, $5, 'image/png',
                           now() - make_interval(hours => $6))";
        let written = query(sql)
            .bind(id)
            .bind(owner.id)
            .bind(storage_key)
            .bind(remote_url)
            .bind(format!("{TEST_BASE_URL}/media/{id}.png"))
            .bind(hours_old)
            .execute(pool)
            .await
            .expect("the media fixture inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a chat between `owner` and `other` and one message from
    /// `owner` carrying `media_id` as its attachment: the stored reference
    /// that keeps an unposted media row out of the sweep's candidates.
    async fn chat_message_with_media(pool: &PgPool, owner: &User, other: &User, media_id: Uuid) {
        let chat = Uuid::now_v7();
        let chat_rows = query("INSERT INTO chats (id, user_id, recipient_id) VALUES ($1, $2, $3)")
            .bind(chat)
            .bind(owner.id)
            .bind(other.id)
            .execute(pool)
            .await
            .expect("the chat fixture inserts");
        assert_eq!(chat_rows.rows_affected(), 1, "the chat row was written");
        let sql = "INSERT INTO chat_messages (id, sender_id, attachment_media_id)
                   VALUES ($1, $2, $3)";
        let message_rows = query(sql)
            .bind(Uuid::now_v7())
            .bind(owner.id)
            .bind(media_id)
            .execute(pool)
            .await
            .expect("the chat message fixture inserts");
        assert_eq!(
            message_rows.rows_affected(),
            1,
            "the message row was written"
        );
    }

    /// Stores `body` at `key` through the test backend and answers whether
    /// the file is on disk under the temporary uploads directory.
    async fn put_media_file(backend: &UploaderBackend, key: &str) {
        backend
            .put(key, Bytes::from_static(b"png bytes"), "image/png")
            .await
            .expect("the upload fixture stores");
    }

    /// Whether `dir` holds a file at relative `key`.
    fn file_on_disk(dir: &std::path::Path, key: &str) -> bool {
        dir.join(key).is_file()
    }

    /// The sweep deletes exactly the stale unposted local orphans: a fresh
    /// orphan (47 h), a stale row with a `remote_url`, and a stale row a
    /// chat message references all stay.
    #[tokio::test]
    async fn the_media_sweep_deletes_only_stale_unposted_local_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let orphan = unposted_media_row(&pool, &alice, None, None, 49).await;
        let fresh = unposted_media_row(&pool, &alice, None, None, 47).await;
        let remote = unposted_media_row(
            &pool,
            &alice,
            None,
            Some("https://remote.example/media/x.png"),
            49,
        )
        .await;
        let in_chat = unposted_media_row(&pool, &alice, None, None, 49).await;
        chat_message_with_media(&pool, &alice, &bob, in_chat).await;

        let outcome = handle_sweep_media(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep over orphans and keeps succeeds");
        assert!(
            !row_exists(&pool, "media_attachments", orphan).await,
            "the stale unposted local row is gone"
        );
        assert!(
            row_exists(&pool, "media_attachments", fresh).await,
            "a 47-hour-old orphan is still inside its posting window"
        );
        assert!(
            row_exists(&pool, "media_attachments", remote).await,
            "a mirrored remote row is not this instance's file to delete"
        );
        assert!(
            row_exists(&pool, "media_attachments", in_chat).await,
            "a chat message still carries the row"
        );
    }

    /// The sweep removes the stored file before it removes the row, so a
    /// reclaimed orphan leaves nothing behind on disk.
    #[tokio::test]
    async fn the_media_sweep_takes_the_stored_file_with_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (dir, state, backend) = uploads_state(&pool);
        let alice = user(&pool, "alice").await;
        let key = format!("alice/{}/stale.png", Uuid::now_v7());
        put_media_file(&backend, &key).await;
        assert!(
            file_on_disk(dir.path(), &key),
            "the fixture file is on disk"
        );
        let orphan = unposted_media_row(&pool, &alice, Some(&key), None, 49).await;

        let outcome = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the sweep over a real file succeeds");
        assert!(
            !row_exists(&pool, "media_attachments", orphan).await,
            "the orphan row is gone"
        );
        assert!(
            !file_on_disk(dir.path(), &key),
            "and its stored file went with it"
        );
    }

    /// A file the uploader refuses to delete (the key names nothing on
    /// disk) keeps its row and costs the pass nothing but the one skip:
    /// the handler still returns `Ok`, and the deleted count covers only
    /// the row it could reclaim, never the kept one.
    #[tokio::test]
    async fn a_row_whose_file_cannot_be_deleted_keeps_its_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (_dir, state, _backend) = uploads_state(&pool);
        let alice = user(&pool, "alice").await;
        let stuck = unposted_media_row(&pool, &alice, Some("alice/missing.png"), None, 49).await;
        let clean = unposted_media_row(&pool, &alice, None, None, 49).await;

        let deleted = sweep_media(&state).await.expect("the sweep runs");

        assert_eq!(
            deleted, 1,
            "the count excludes the row whose file delete failed"
        );
        assert!(
            !row_exists(&pool, "media_attachments", clean).await,
            "the row with nothing to remove went"
        );
        assert!(
            row_exists(&pool, "media_attachments", stuck).await,
            "the stuck row waits for the next run"
        );

        let outcome = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;
        assert_eq!(
            outcome,
            Ok(()),
            "an undeletable file is a warn, not a failure"
        );
        assert!(
            row_exists(&pool, "media_attachments", stuck).await,
            "and the job run keeps skipping it without going down"
        );
    }

    /// A row with no `storage_key` has no file step: it goes directly.
    #[tokio::test]
    async fn a_row_without_a_storage_key_deletes_without_a_file_step() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (dir, state, _backend) = uploads_state(&pool);
        let alice = user(&pool, "alice").await;
        let keyless = unposted_media_row(&pool, &alice, None, None, 49).await;

        let outcome = handle_sweep_media(state, FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the keyless row is deletable bookkeeping");
        assert!(
            !row_exists(&pool, "media_attachments", keyless).await,
            "it goes without any file"
        );
        let leftovers = dir
            .path()
            .read_dir()
            .expect("the fresh uploads dir is readable")
            .count();
        assert_eq!(leftovers, 0, "the sweep created no stray file for it");
    }

    /// A backlog larger than one batch is swept by a single handler run:
    /// the id cursor walks every batch in one pass, and a row kept after a
    /// failed file delete does not re-loop inside it.
    #[tokio::test]
    async fn the_media_sweep_walks_more_than_one_batch_in_one_run() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let want = u64::try_from(MEDIA_BATCH_SIZE + 5).expect("the batch size is positive");
        let inserted = query(
            "INSERT INTO media_attachments (id, user_id, url, content_type, inserted_at)
             SELECT gen_random_uuid(), $1, 'https://test.example/media/bulk.png', 'image/png',
                    now() - make_interval(hours => 49)
               FROM generate_series(1, $2)",
        )
        .bind(alice.id)
        .bind(MEDIA_BATCH_SIZE + 5)
        .execute(&pool)
        .await
        .expect("the bulk media fixture inserts");
        assert_eq!(
            inserted.rows_affected(),
            want,
            "every seeded orphan landed in the table"
        );

        let outcome = handle_sweep_media(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the multi-batch pass succeeds");
        let left = query_scalar::<_, i64>(
            "SELECT count(*) FROM media_attachments
              WHERE status_id IS NULL AND remote_url IS NULL",
        )
        .fetch_one(&pool)
        .await
        .expect("the rows read");
        assert_eq!(
            left, 0,
            "one handler run cleared more than {MEDIA_BATCH_SIZE} rows: \
             the cursor walked, not a single batch"
        );
    }

    /// The media sweep is wired into both registries the ticket names: the
    /// worker can run its kind, and the scheduler places it on the
    /// maintenance queue every six hours.
    #[test]
    fn the_media_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_MEDIA).is_some(),
            "the worker knows `{KIND_MEDIA}`"
        );
        assert_eq!(KIND_MEDIA, "maintenance.media", "the ticket's kind name");

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_MEDIA)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, SIX_HOURS, "once every six hours");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_sweep_media`], not some other sweep: calling the registered
    /// handler reclaims the stale orphan and keeps the fresh one.
    #[tokio::test]
    async fn the_registered_media_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let orphan = unposted_media_row(&pool, &alice, None, None, 49).await;
        let fresh = unposted_media_row(&pool, &alice, None, None, 47).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_MEDIA)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        assert!(
            !row_exists(&pool, "media_attachments", orphan).await,
            "and it is this module's sweep: the stale orphan is gone"
        );
        assert!(
            row_exists(&pool, "media_attachments", fresh).await,
            "and only the stale rows went: the fresh one stays"
        );
    }

    /// A sweep that cannot reach the database is a retryable failure, so
    /// the queue keeps the pass while the scheduler enqueues the next
    /// period: the stale rows wait for a working sweep instead of being
    /// assumed gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_media_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let orphan = unposted_media_row(&pool, &alice, Some("alice/orphan.png"), None, 49).await;
        let broken = broken_state().await;

        let outcome = handle_sweep_media(broken, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("orphaned media sweep failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            row_exists(&pool, "media_attachments", orphan).await,
            "the healthy pool sees no work the broken one claimed to do"
        );
    }
}
