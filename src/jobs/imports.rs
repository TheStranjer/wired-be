//! The user-import jobs behind the CSV import endpoints.
//!
//! `/api/pleroma/follow_import`, `/api/pleroma/blocks_import` and
//! `/api/pleroma/mutes_import` (WBE-138) mirror Pleroma's
//! `User.Import` + `BackgroundWorker` pair
//! (`lib/pleroma/user/import.ex`).
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres
//! rows: [`crate::api::pleroma_import`] parses the uploaded list and
//! enqueues one job per import on [`QUEUE_BACKGROUND`] — kind
//! [`KIND_FOLLOWS`], [`KIND_BLOCKS`] or [`KIND_MUTES`], args
//! `{user_id, accts, options}` (the document [`job_args`] builds and
//! [`ImportArgs`] deserialises) — and this module runs them. Per entry the
//! handler resolves the account through [`get_or_fetch_by_acct`] and
//! applies the relationship with the standard model functions
//! ([`follow::follow`] plus [`follow::set_toggles`] for the `Show boosts`
//! column, [`block::block`], [`mute::mute`]), firing the same
//! `after_follow_changed` / `after_block_changed` hooks the
//! interactive endpoints fire. A resolution failure or a refusal (the
//! self-action, a blocked pair) is logged and the entry skipped, as in
//! Pleroma's `handle_error`; only a database failure retries the job —
//! every import write is idempotent, so the rerun converges.
//!
//! Pleroma enqueues one Oban job per acct; this server enqueues one job
//! per import and lets the follow handler throttle itself: each run
//! processes the first `FOLLOW_CHUNK` entries in order and re-enqueues
//! the remainder as a fresh [`KIND_FOLLOWS`] job `FOLLOW_CHUNK_DELAY`
//! later. Blocks and mutes run their whole list in one pass.
//!
//! [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
//! [`follow::follow`]: crate::models::follow::follow
//! [`follow::set_toggles`]: crate::models::follow::set_toggles
//! [`block::block`]: crate::models::block::block
//! [`mute::mute`]: crate::models::mute::mute

use chrono::{TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::{accounts::after_follow_changed, blocks::after_block_changed},
    error::AppError,
    federation::fetch_actor::get_or_fetch_by_acct,
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_BACKGROUND},
    models::{
        block,
        follow::{self, FollowToggles},
        job::{NewJob, enqueue},
        mute::{self, MuteOptions},
        user::{self, AccountStatus, User},
    },
    state::AppState,
};

/// The job kind of a follow import, handled by [`handle_follows`] on
/// [`QUEUE_BACKGROUND`].
pub const KIND_FOLLOWS: &str = "imports.follows";
/// The job kind of a block import, handled by [`handle_blocks`] on
/// [`QUEUE_BACKGROUND`].
pub const KIND_BLOCKS: &str = "imports.blocks";
/// The job kind of a mute import, handled by [`handle_mutes`] on
/// [`QUEUE_BACKGROUND`].
pub const KIND_MUTES: &str = "imports.mutes";
/// Entries one [`handle_follows`] run processes before re-enqueuing the
/// rest (the ticket's chunk size of 50).
const FOLLOW_CHUNK: usize = 50;
/// How long the re-enqueued remainder of a follow import waits before its
/// next chunk may run (the ticket's 5 s throttle).
const FOLLOW_CHUNK_DELAY: TimeDelta = TimeDelta::seconds(5);

/// The `{user_id, accts, options}` arguments of an import job.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ImportArgs {
    /// The importing account's id.
    pub user_id: Uuid,
    /// The account handles to act on, in list order.
    pub accts: Vec<String>,
    /// Per-account options, parallel to [`accts`](Self::accts) for a
    /// follow import; empty for blocks and mutes.
    #[serde(default)]
    pub options: Vec<ImportOption>,
}

/// The per-entry option set of a follow import: an entry with no `Show
/// boosts` column, and every entry of the kinds that carry no options at
/// all, is the all-`None` default.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImportOption {
    /// Mastodon's `Show boosts` column, written as the row's
    /// [`Follow::show_reblogs`](crate::models::follow::Follow::show_reblogs)
    /// toggle through [`follow::set_toggles`]; `None` keeps the column
    /// default a fresh row carries (`true`).
    #[serde(default)]
    pub show_reblogs: Option<bool>,
}

impl ImportArgs {
    /// Reads the arguments of a claimed job; a payload this shape cannot
    /// deserialize is permanent — the job is discarded, never retried.
    fn from_value(args: &Value) -> Result<Self, JobError> {
        serde_json::from_value(args.clone())
            .map_err(|err| JobError::Discard(format!("import job arguments are unusable: {err}")))
    }

    /// The same arguments with [`options`](Self::options) padded or
    /// truncated to exactly one entry per acct, so the two lists always
    /// zip, and always split at the same index.
    fn with_options_aligned(mut self) -> Self {
        self.options
            .resize_with(self.accts.len(), ImportOption::default);
        self
    }
}

/// The `{user_id, accts, options}` job-arguments document: the one
/// shape [`crate::api::pleroma_import`] enqueues and [`handle_follows`]
/// re-enqueues.
#[must_use]
pub fn job_args(user_id: Uuid, accts: &[String], options: &[ImportOption]) -> Value {
    serde_json::json!({
        "user_id": user_id,
        "accts": accts,
        "options": options,
    })
}

/// The relationship write one import job performs per entry; the kinds
/// differ only in which model function and hook they run.
#[derive(Clone, Copy, Debug)]
enum Action {
    /// [`follow::follow`], the [`follow::set_toggles`] of the row's `Show
    /// boosts` value, and the `after_follow_changed` hook.
    Follow,
    /// [`block::block`] and the `after_block_changed` hook.
    Block,
    /// [`mute::mute`] with the Mastodon defaults.
    Mute,
}

impl Action {
    /// The action's name for the skip logs.
    const fn name(self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::Block => "block",
            Self::Mute => "mute",
        }
    }
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_FOLLOWS`].
///
/// Applies the first `FOLLOW_CHUNK` entries in order, then re-enqueues
/// the remainder on [`QUEUE_BACKGROUND`] due `FOLLOW_CHUNK_DELAY` later
/// — the follow import throttles itself.
pub fn handle_follows(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let parsed = ImportArgs::from_value(&args)?.with_options_aligned();
        let importer = load_importer(&state, parsed.user_id).await?;
        let head_count = parsed.accts.len().min(FOLLOW_CHUNK);
        let mut head = parsed;
        let tail_accts = head.accts.split_off(head_count);
        let tail_options = head.options.split_off(head_count);
        apply_entries(&state, &importer, Action::Follow, &head).await?;
        if !tail_accts.is_empty() {
            enqueue_next_chunk(&state, head.user_id, tail_accts, tail_options).await?;
        }
        Ok(())
    })
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_BLOCKS`]:
/// block every acct of the list in one pass.
pub fn handle_blocks(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let parsed = ImportArgs::from_value(&args)?.with_options_aligned();
        let importer = load_importer(&state, parsed.user_id).await?;
        apply_entries(&state, &importer, Action::Block, &parsed).await
    })
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_MUTES`]:
/// mute every acct of the list in one pass.
pub fn handle_mutes(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let parsed = ImportArgs::from_value(&args)?.with_options_aligned();
        let importer = load_importer(&state, parsed.user_id).await?;
        apply_entries(&state, &importer, Action::Mute, &parsed).await
    })
}

/// The importer the job acts as. A vanished user, or one that can no
/// longer act (deactivated, deleted, or awaiting approval), is permanent
/// (discard); a database that cannot answer is a retry.
///
/// # Errors
///
/// [`JobError::Discard`] when the user row is gone or the account can no
/// longer act, [`JobError::Retry`] on a database failure.
async fn load_importer(state: &AppState, user_id: Uuid) -> Result<User, JobError> {
    match user::get_by_id(&state.db, user_id).await {
        Ok(Some(importer)) => {
            let blocked = matches!(
                importer.account_status(),
                AccountStatus::Deactivated | AccountStatus::ApprovalPending
            ) || importer.deleted_at.is_some();
            if blocked {
                return Err(JobError::Discard(format!(
                    "the import job's user {user_id} can no longer act"
                )));
            }
            Ok(importer)
        }
        Ok(None) => Err(JobError::Discard(format!(
            "the import job's user {user_id} no longer exists"
        ))),
        Err(err) => Err(JobError::Retry(format!("import user read failed: {err}"))),
    }
}

/// Runs `action` for every (acct, option) pair of `args`, in list order.
///
/// # Errors
///
/// [`JobError::Retry`] when an entry failed on the database (see
/// [`skip_or_retry`]).
async fn apply_entries(
    state: &AppState,
    importer: &User,
    action: Action,
    args: &ImportArgs,
) -> Result<(), JobError> {
    for (acct, option) in args.accts.iter().zip(&args.options) {
        apply_one(state, importer, action, acct, option).await?;
    }
    Ok(())
}

/// Resolves `acct` and applies `action` to the pair. What the entry
/// cannot survive is logged and skipped; only database failures travel up
/// as a retry (Pleroma runs its accts as independent jobs; the in-order
/// walk here is what lets the follow chunk promise an ordered remainder).
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure inside the write.
async fn apply_one(
    state: &AppState,
    importer: &User,
    action: Action,
    acct: &str,
    option: &ImportOption,
) -> Result<(), JobError> {
    let target = match get_or_fetch_by_acct(state, acct).await {
        Ok(target) => target,
        Err(err) => {
            debug!(
                %err,
                acct,
                action = action.name(),
                "import entry skipped: the account could not be resolved"
            );
            return Ok(());
        }
    };
    match action {
        Action::Follow => apply_follow(state, importer, &target, option).await,
        Action::Block => apply_block(state, importer, &target).await,
        Action::Mute => apply_mute(state, importer, &target).await,
    }
}

/// Follows `target`, stores the entry's `Show boosts` value, and fires
/// the same [`after_follow_changed`] hook `POST /api/v1/accounts/{id}/follow`
/// fires.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure; a logged refusal is the `Ok`
/// skip.
async fn apply_follow(
    state: &AppState,
    importer: &User,
    target: &User,
    option: &ImportOption,
) -> Result<(), JobError> {
    let created = match follow::follow(&state.db, &state.config.base_url, importer, target).await {
        Ok(created) => created,
        Err(err) => return skip_or_retry(err, Action::Follow, target),
    };
    let stored = match option.show_reblogs {
        Some(show_reblogs) => {
            let toggles = FollowToggles {
                show_reblogs: Some(show_reblogs),
                notify: None,
            };
            let updated = match follow::set_toggles(
                &state.db,
                created.follower_id,
                created.followee_id,
                toggles,
            )
            .await
            {
                Ok(updated) => updated,
                Err(err) => return skip_or_retry(err, Action::Follow, target),
            };
            updated.unwrap_or(created)
        }
        None => created,
    };
    after_follow_changed(state, importer, target, Some(&stored), true).await;
    Ok(())
}

/// Blocks `target`, firing the same [`after_block_changed`] hook
/// `POST /api/v1/accounts/{id}/block` fires.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure; a logged refusal is the `Ok`
/// skip.
async fn apply_block(state: &AppState, importer: &User, target: &User) -> Result<(), JobError> {
    let created = match block::block(&state.db, &state.config.base_url, importer, target).await {
        Ok(created) => created,
        Err(err) => return skip_or_retry(err, Action::Block, target),
    };
    after_block_changed(state, &created, true).await;
    Ok(())
}

/// Mutes `target` with the Mastodon defaults (notifications hidden, no
/// expiry), the write `POST /api/v1/accounts/{id}/mute` performs.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure; a logged refusal is the `Ok`
/// skip.
async fn apply_mute(state: &AppState, importer: &User, target: &User) -> Result<(), JobError> {
    match mute::mute(&state.db, importer, target, MuteOptions::default()).await {
        Ok(row) => {
            debug!(mute = %row.id, target = %target.nickname, "import mute applied");
            Ok(())
        }
        Err(err) => skip_or_retry(err, Action::Mute, target),
    }
}

/// One model-function failure's treatment: a database failure becomes a
/// [`JobError::Retry`] (the job re-runs, the writes are idempotent), any
/// other refusal — the self-action, a blocked pair — is Pleroma's
/// `handle_error`: logged, the entry skipped, `Ok` to keep the walk going.
///
/// The returned `Result` is what lets a caller `return` it from an `Ok`
/// skip as well as from a retry.
fn skip_or_retry(err: AppError, action: Action, target: &User) -> Result<(), JobError> {
    if let AppError::Database(detail) = err {
        return Err(JobError::Retry(format!(
            "{} import write failed: {detail}",
            action.name()
        )));
    }
    debug!(
        %err,
        action = action.name(),
        target = %target.nickname,
        "import entry skipped: the action was refused"
    );
    Ok(())
}

/// Re-enqueues the not-yet-processed remainder of a follow import as a
/// fresh [`KIND_FOLLOWS`] job on [`QUEUE_BACKGROUND`], due
/// [`FOLLOW_CHUNK_DELAY`] from now.
///
/// # Errors
///
/// [`JobError::Retry`] when the job row cannot be written.
async fn enqueue_next_chunk(
    state: &AppState,
    user_id: Uuid,
    accts: Vec<String>,
    options: Vec<ImportOption>,
) -> Result<(), JobError> {
    let queued = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: KIND_FOLLOWS,
        args: job_args(user_id, &accts, &options),
        run_at: Some(Utc::now() + FOLLOW_CHUNK_DELAY),
        max_attempts: None,
        unique_key: None,
    };
    let _job = enqueue(&state.db, queued).await.map_err(|err| {
        JobError::Retry(format!("follow import chunk could not be enqueued: {err}"))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::job::Job,
        models::user::{NewUser, insert, local_ap_id},
    };

    /// A handle that neither the database nor a fast-failing `WebFinger`
    /// check can resolve (single-label domain), the skip case.
    const UNRESOLVABLE: &str = "ghost@invalid";

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        AppState::for_tests(pool.clone(), Config::for_tests())
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
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a mirror of `nickname` on `host`: the shape of a remote
    /// account the fetch would store, so a handler entry resolves from the
    /// database without any network (the ticket's mock-remote-fixture).
    async fn remote_user(pool: &PgPool, nickname: &str, host: &str) -> User {
        let ap_id = format!("https://{host}/users/{nickname}");
        let new = NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id,
            domain: Some(host.to_owned()),
            name: nickname.to_owned(),
            ..NewUser::default()
        };
        insert(pool, &new)
            .await
            .expect("remote user fixture inserts")
    }

    /// The handler's attempt info: imports do not depend on the attempt.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    /// The stored job rows of `kind`, oldest first.
    async fn jobs_of(pool: &PgPool, kind: &str) -> Vec<Job> {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
            .bind(kind)
            .fetch_all(pool)
            .await
            .expect("the job rows read")
    }

    /// The follow row of the pair, which the test expects to exist.
    async fn relation(pool: &PgPool, follower: &User, target: &User) -> follow::Follow {
        follow::get(pool, follower.id, target.id)
            .await
            .unwrap()
            .expect("the relationship exists")
    }

    /// Whether the pair has any follow row.
    async fn relation_exists(pool: &PgPool, follower: &User, target: &User) -> bool {
        follow::get(pool, follower.id, target.id)
            .await
            .unwrap()
            .is_some()
    }

    /// The follow handler follows resolvable accts — a stored local and a
    /// stored remote one, the mock-remote-fixture case — writes the
    /// `Show boosts` column it was given, and logs-and-skips an acct
    /// nothing resolves.
    #[tokio::test]
    async fn follows_apply_the_list_and_skip_unresolvable_accts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob", "remote.example").await;
        let carol = local_user(&pool, "carol").await;
        let args = json!({
            "user_id": alice.id,
            "accts": [bob.nickname, carol.nickname, UNRESOLVABLE],
            "options": [{"show_reblogs": false}, {}, {"show_reblogs": true}],
        });

        let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

        assert_eq!(outcome, Ok(()), "a skipped entry never fails the job");
        let bob_follow = relation(&pool, &alice, &bob).await;
        assert!(!bob_follow.show_reblogs, "the False column reaches the row");
        assert_eq!(
            bob_follow.state,
            follow::FollowState::Pending,
            "a mirror of a remote account waits for that instance, as the \
             interactive follow does"
        );
        assert!(
            relation(&pool, &alice, &carol).await.show_reblogs,
            "an entry without a value keeps the default"
        );
        assert!(
            follow::is_following(&pool, &alice, &carol).await.unwrap(),
            "the local acct's follow was granted on the spot"
        );
        assert!(
            jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
            "a list shorter than the chunk leaves no remainder job"
        );
    }

    /// The follow handler processes the first 50 entries and re-enqueues
    /// the remainder (in order, with the aligned options) due 5 s later.
    #[tokio::test]
    async fn follows_chunk_at_fifty_and_reenqueue_the_remainder() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let mut names: Vec<String> = Vec::new();
        let mut users = Vec::new();
        for i in 0..60 {
            let name = format!("user{i}@chunk.example");
            users.push(remote_user(&pool, &format!("user{i}"), "chunk.example").await);
            names.push(name);
        }
        let options: Vec<Value> = names.iter().map(|_| json!({})).collect();
        let args = json!({"user_id": alice.id, "accts": names, "options": options});

        let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

        assert_eq!(outcome, Ok(()), "the first chunk runs clean");
        for user in users.iter().take(50) {
            assert!(
                relation_exists(&pool, &alice, user).await,
                "the first chunk followed {}",
                user.nickname
            );
        }
        for user in users.iter().skip(50) {
            assert!(
                !relation_exists(&pool, &alice, user).await,
                "the remainder is left for the next chunk: {}",
                user.nickname
            );
        }
        let pending = jobs_of(&pool, KIND_FOLLOWS).await;
        assert_eq!(pending.len(), 1, "exactly one remainder job");
        let next = &pending[0];
        assert_eq!(next.queue, QUEUE_BACKGROUND, "on the background queue");
        assert_eq!(
            next.args
                .get("accts")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(10),
            "the remainder carries the last ten accts, in order: {}",
            next.args
        );
        assert_eq!(
            next.args["accts"][0],
            json!("user50@chunk.example"),
            "and it starts exactly where this run stopped"
        );
        assert_eq!(
            next.args["options"].as_array().map_or_default(Vec::len),
            10,
            "the options travel with the accts"
        );
        assert!(
            next.run_at > Utc::now(),
            "the chunk is throttled into the future: {}",
            next.run_at
        );
        assert!(
            next.run_at < Utc::now() + FOLLOW_CHUNK_DELAY + FOLLOW_CHUNK_DELAY,
            "the throttle is the ticket's 5 s, not a backoff: {}",
            next.run_at
        );
    }

    /// The second chunk of the ticket's own throttling: running the
    /// re-enqueued job applies its remainder and enqueues nothing more.
    #[tokio::test]
    async fn the_remainder_chunk_completes_the_import() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob", "remote.example").await;
        let args = json!({
            "user_id": alice.id,
            "accts": [bob.nickname],
            "options": [{"show_reblogs": true}],
        });

        let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

        assert_eq!(outcome, Ok(()), "a small job is its own whole chunk");
        assert!(
            relation_exists(&pool, &alice, &bob).await,
            "and it followed"
        );
        assert!(
            jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
            "no remainder"
        );
    }

    /// The blocks handler blocks resolvable accts — local and mirrored
    /// remote — logs-and-skips the rest, and is idempotent across runs.
    #[tokio::test]
    async fn blocks_apply_to_resolvable_accts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob", "remote.example").await;
        let args = json!({
            "user_id": alice.id,
            "accts": [bob.nickname, alice.nickname, UNRESOLVABLE],
            "options": [],
        });
        let state = app_state(&pool);

        let outcome = handle_blocks(state.clone(), FIRST_ATTEMPT, args.clone()).await;
        assert_eq!(outcome, Ok(()), "the self refusal and the skip pass");
        assert!(
            block::is_blocking(&pool, alice.id, bob.id).await.unwrap(),
            "the stored remote acct is blocked"
        );
        assert!(
            !block::is_blocking(&pool, alice.id, alice.id).await.unwrap(),
            "importing yourself is skipped, not written"
        );

        let rerun = handle_blocks(state, FIRST_ATTEMPT, args).await;
        assert_eq!(
            rerun,
            Ok(()),
            "a second run changes nothing and fails nowhere"
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM blocks")
            .fetch_one(&pool)
            .await
            .expect("the count reads");
        assert_eq!(rows, 1, "idempotent: still exactly one block row");
    }

    /// The mutes handler mutes resolvable accts with the Mastodon
    /// defaults (notifications hidden, indefinite) and skips what it
    /// cannot resolve.
    #[tokio::test]
    async fn mutes_apply_to_resolvable_accts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob", "remote.example").await;
        let args = json!({
            "user_id": alice.id,
            "accts": [bob.nickname, UNRESOLVABLE],
            "options": [],
        });

        let outcome = handle_mutes(app_state(&pool), FIRST_ATTEMPT, args).await;

        assert_eq!(outcome, Ok(()), "the unresolvable entry is a skip");
        assert!(
            mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "the stored remote acct is muted"
        );
        let expires: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT expires_at FROM mutes WHERE muter_id = $1")
                .bind(alice.id)
                .fetch_one(&pool)
                .await
                .expect("the mute row reads");
        assert_eq!(expires, None, "an import mute is indefinite");
    }

    /// A blocked pair is Pleroma's logged skip, not a job failure: the
    /// follow of an account that blocks the importer keeps the walk going.
    #[tokio::test]
    async fn a_blocked_target_is_skipped_not_fatal() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let _blocked = block::block(&pool, TEST_BASE_URL, &bob, &alice)
            .await
            .expect("bob blocks alice");
        let carol = local_user(&pool, "carol").await;
        let args = json!({
            "user_id": alice.id,
            "accts": [bob.nickname, carol.nickname],
            "options": [],
        });

        let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

        assert_eq!(outcome, Ok(()), "the refusal is logged and skipped");
        assert!(
            !follow::is_following(&pool, &alice, &bob).await.unwrap(),
            "the blocking account was not followed"
        );
        assert!(
            follow::is_following(&pool, &alice, &carol).await.unwrap(),
            "and the list walk continued to the next entry"
        );
    }

    /// Arguments that do not fit [`ImportArgs`] are permanent: discard.
    #[tokio::test]
    async fn unusable_arguments_discard_the_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, json!({"accts": []})).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("unusable"),
                "the discard says why: {detail}"
            ),
            Ok(()) => panic!("a missing user_id must not complete the job"),
            Err(other) => panic!("a missing user_id must discard, not {other}"),
        }
    }

    /// A user the database no longer has is permanent: discard.
    #[tokio::test]
    async fn a_vanished_user_discards_the_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let args = job_args(Uuid::now_v7(), &[], &[]);

        let outcome = handle_blocks(app_state(&pool), FIRST_ATTEMPT, args).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("no longer exists"),
                "the discard says why: {detail}"
            ),
            Ok(()) => panic!("a vanished user must not complete the job"),
            Err(other) => panic!("a vanished user must discard, not {other}"),
        }
    }

    /// Asserts `outcome` is the "can no longer act" discard.
    fn assert_cannot_act(outcome: Result<(), JobError>) {
        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("can no longer act"),
                "the discard says why: {detail}"
            ),
            Ok(()) => panic!("an inactive importer must not complete the job"),
            Err(other) => panic!("an inactive importer must discard, not {other}"),
        }
    }

    /// A deleted importer's follow import is discarded: no follow row, and
    /// no next chunk even when the list exceeds one chunk.
    #[tokio::test]
    async fn a_deleted_importer_follows_nothing_and_ends_the_chain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let mut names: Vec<String> = Vec::new();
        let mut users = Vec::new();
        for i in 0..60 {
            users.push(remote_user(&pool, &format!("user{i}"), "chunk.example").await);
            names.push(format!("user{i}@chunk.example"));
        }
        user::mark_deleted(&pool, alice.id)
            .await
            .expect("alice is marked deleted");
        let args = job_args(alice.id, &names, &[]);

        let outcome = handle_follows(app_state(&pool), FIRST_ATTEMPT, args).await;

        assert_cannot_act(outcome);
        for target in &users {
            assert!(
                !relation_exists(&pool, &alice, target).await,
                "no follow row for {}",
                target.nickname
            );
        }
        assert!(
            jobs_of(&pool, KIND_FOLLOWS).await.is_empty(),
            "no next chunk is enqueued"
        );
    }

    /// A deactivated importer's block and mute imports are discarded and
    /// write nothing.
    #[tokio::test]
    async fn a_deactivated_importer_blocks_and_mutes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob", "remote.example").await;
        sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .map(drop)
            .expect("alice is deactivated");
        let args = job_args(alice.id, std::slice::from_ref(&bob.nickname), &[]);
        let state = app_state(&pool);

        assert_cannot_act(handle_blocks(state.clone(), FIRST_ATTEMPT, args.clone()).await);
        assert_cannot_act(handle_mutes(state, FIRST_ATTEMPT, args).await);

        assert!(
            !block::is_blocking(&pool, alice.id, bob.id).await.unwrap(),
            "no block was written"
        );
        assert!(
            !mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "no mute was written"
        );
    }

    /// An importer awaiting staff approval cannot act either.
    #[tokio::test]
    async fn an_unapproved_importer_is_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        sqlx::query("UPDATE users SET approval_pending = true WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .map(drop)
            .expect("alice awaits approval");

        let outcome = handle_blocks(
            app_state(&pool),
            FIRST_ATTEMPT,
            job_args(alice.id, &[], &[]),
        )
        .await;

        assert_cannot_act(outcome);
    }

    /// An active importer still applies its entries.
    #[tokio::test]
    async fn an_active_importer_still_applies_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob", "remote.example").await;
        let carol = remote_user(&pool, "carol", "remote.example").await;
        let dave = remote_user(&pool, "dave", "remote.example").await;
        let state = app_state(&pool);
        let args_for =
            |target: &User| job_args(alice.id, std::slice::from_ref(&target.nickname), &[]);

        assert_eq!(
            handle_follows(state.clone(), FIRST_ATTEMPT, args_for(&bob)).await,
            Ok(())
        );
        assert_eq!(
            handle_blocks(state.clone(), FIRST_ATTEMPT, args_for(&carol)).await,
            Ok(())
        );
        assert_eq!(
            handle_mutes(state, FIRST_ATTEMPT, args_for(&dave)).await,
            Ok(())
        );

        assert!(
            relation_exists(&pool, &alice, &bob).await,
            "the follow was written"
        );
        assert!(
            block::is_blocking(&pool, alice.id, carol.id).await.unwrap(),
            "the block was written"
        );
        assert!(
            mute::is_muting(&pool, alice.id, dave.id).await.unwrap(),
            "the mute was written"
        );
    }

    /// A database that cannot answer is transient: retry, so the import
    /// resumes with its idempotent writes once the database is back.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_job() {
        let args = json!({"user_id": Uuid::now_v7().to_string(), "accts": [], "options": []});
        let state = broken_state().await;

        let outcome = handle_mutes(state, FIRST_ATTEMPT, args).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.contains("import user read failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful import"),
            Err(other) => panic!("a closed pool is retryable, not {other}"),
        }
    }

    /// All three kinds are registered with the worker's registry, each
    /// bound to this module's handler.
    #[tokio::test]
    async fn the_kinds_are_registered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let registry = crate::job_registry();
        let alice = local_user(&pool, "alice").await;
        let args = job_args(alice.id, &[], &[]);
        for kind in [KIND_FOLLOWS, KIND_BLOCKS, KIND_MUTES] {
            let handler = registry
                .get(kind)
                .unwrap_or_else(|| panic!("`{kind}` is registered"));
            let outcome = handler(app_state(&pool), FIRST_ATTEMPT, args.clone()).await;
            assert_eq!(outcome, Ok(()), "the registered handler for `{kind}` runs");
        }
    }
}
