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
//! per import and lets every handler throttle itself: each run
//! processes the first `IMPORT_CHUNK` entries in order and re-enqueues
//! the remainder as a fresh job of the same kind `IMPORT_CHUNK_DELAY`
//! later, so no job outlives the stuck-lock age however long the list.
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
/// Entries one import run ([`handle_follows`], [`handle_blocks`] or
/// [`handle_mutes`]) processes before re-enqueuing the rest.
const IMPORT_CHUNK: usize = 50;
/// How long the re-enqueued remainder of an import waits before its next
/// chunk may run (the 5 s throttle).
const IMPORT_CHUNK_DELAY: TimeDelta = TimeDelta::seconds(5);

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

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_FOLLOWS`]:
/// one chunk of follows per run, the rest re-enqueued.
pub fn handle_follows(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_chunked(&state, &args, Action::Follow, KIND_FOLLOWS).await })
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_BLOCKS`]:
/// one chunk of blocks per run, the rest re-enqueued.
pub fn handle_blocks(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_chunked(&state, &args, Action::Block, KIND_BLOCKS).await })
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_MUTES`]:
/// one chunk of mutes per run, the rest re-enqueued.
pub fn handle_mutes(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_chunked(&state, &args, Action::Mute, KIND_MUTES).await })
}

/// Applies the first [`IMPORT_CHUNK`] entries of `args` in order with
/// `action`, then re-enqueues the remainder as a fresh `kind` job on
/// [`QUEUE_BACKGROUND`] due [`IMPORT_CHUNK_DELAY`] later, so no job runs
/// longer than one chunk of resolutions.
///
/// # Errors
///
/// [`JobError::Discard`] for unusable arguments or an importer that can no
/// longer act, [`JobError::Retry`] on a database failure.
async fn handle_chunked(
    state: &AppState,
    args: &Value,
    action: Action,
    kind: &'static str,
) -> Result<(), JobError> {
    let parsed = ImportArgs::from_value(args)?.with_options_aligned();
    let importer = load_importer(state, parsed.user_id).await?;
    let head_count = parsed.accts.len().min(IMPORT_CHUNK);
    let mut head = parsed;
    let tail_accts = head.accts.split_off(head_count);
    let tail_options = head.options.split_off(head_count);
    apply_entries(state, &importer, action, &head).await?;
    if !tail_accts.is_empty() {
        enqueue_next_chunk(state, kind, head.user_id, tail_accts, tail_options).await?;
    }
    Ok(())
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

/// Re-enqueues the not-yet-processed remainder of an import as a fresh
/// `kind` ([`KIND_FOLLOWS`], [`KIND_BLOCKS`] or [`KIND_MUTES`]) job on
/// [`QUEUE_BACKGROUND`], due [`IMPORT_CHUNK_DELAY`] from now.
///
/// # Errors
///
/// [`JobError::Retry`] when the job row cannot be written.
async fn enqueue_next_chunk(
    state: &AppState,
    kind: &'static str,
    user_id: Uuid,
    accts: Vec<String>,
    options: Vec<ImportOption>,
) -> Result<(), JobError> {
    let queued = NewJob {
        queue: QUEUE_BACKGROUND,
        kind,
        args: job_args(user_id, &accts, &options),
        run_at: Some(Utc::now() + IMPORT_CHUNK_DELAY),
        max_attempts: None,
        unique_key: None,
    };
    let _job = enqueue(&state.db, queued)
        .await
        .map_err(|err| JobError::Retry(format!("{kind} chunk could not be enqueued: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests;
