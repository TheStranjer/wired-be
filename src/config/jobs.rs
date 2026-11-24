//! Reads the job worker's settings, the maintenance purge age, the backup
//! trio, the streaming heartbeat, and the `RestSettings` bundle that
//! flattens the themeless reader groups for [`Config::from_lookup`].

#[cfg(doc)]
use super::Config;

use crate::{extract::multipart::UploadLimits, jobs::DEFAULT_JOB_QUEUES};

use super::{
    defaults::{
        DEFAULT_BACKUP_LIMIT_DAYS, DEFAULT_BACKUP_PURGE, DEFAULT_BACKUP_VALIDITY_DAYS,
        DEFAULT_JOB_CONCURRENCY, DEFAULT_PURGE_DELETED_STATUSES_DAYS,
        DEFAULT_STREAMING_PING_INTERVAL_SECS,
    },
    error::ConfigError,
    federation_types::{FetchSettings, Toggle},
    fetch::fetch_settings,
    parse::{boolean, comma_list, number},
    uploads::{misc_toggles, upload_settings},
    vars::{
        BACKUP_LIMIT_DAYS_VAR, BACKUP_PURGE_VAR, BACKUP_VALIDITY_DAYS_VAR, JOB_CONCURRENCY_VAR,
        JOB_QUEUES_VAR, PURGE_DELETED_STATUSES_DAYS_VAR, STREAMING_PING_INTERVAL_SECS_VAR,
    },
};

/// The shape [`job_worker`] answers: the worker's concurrent handler count,
/// its queue names in claim order, and the age in days after which the
/// daily sweep hard-deletes soft-deleted local statuses; factored out
/// because a mixed tuple of queues and numbers reads better by name (and a
/// bare `Result` of it trips `type_complexity`).
type JobWorkerSettings = (usize, Vec<String>, usize);

/// Reads the three settings that shape the job worker and its maintenance
/// purge: how many handlers a single worker runs at the same time, the
/// queue names it drains, in claim priority order, and the age in days
/// after which the daily sweep hard-deletes soft-deleted local statuses
/// ([`PURGE_DELETED_STATUSES_DAYS_VAR`], `0` disabling the purge). Grouped
/// because each is a plain default-or-parse and [`Config::from_lookup`]
/// has a size budget (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when `JOB_CONCURRENCY` or
/// `PURGE_DELETED_STATUSES_DAYS` holds a value that is not a whole number.
fn job_worker<F>(lookup: &F) -> Result<JobWorkerSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let job_concurrency = number(lookup, JOB_CONCURRENCY_VAR, DEFAULT_JOB_CONCURRENCY)?;
    let job_queues =
        comma_list(&lookup(JOB_QUEUES_VAR).unwrap_or_else(|| DEFAULT_JOB_QUEUES.to_owned()));
    let purge_deleted_statuses_days = number(
        lookup,
        PURGE_DELETED_STATUSES_DAYS_VAR,
        DEFAULT_PURGE_DELETED_STATUSES_DAYS,
    )?;
    Ok((job_concurrency, job_queues, purge_deleted_statuses_days))
}

/// Reads the three settings that shape the user-backup feature: the age in
/// days a finished backup archive stays downloadable
/// ([`BACKUP_VALIDITY_DAYS_VAR`]), the minimum number of days between two
/// backups of one user ([`BACKUP_LIMIT_DAYS_VAR`]) and whether the backup
/// purge sweep runs at all ([`BACKUP_PURGE_VAR`]). Grouped because each is a
/// plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when one of the two day settings
/// holds a value that is not a whole number, and
/// [`ConfigError::InvalidBoolean`] when `BACKUP_PURGE` holds a value that is
/// neither spelling.
fn backup_settings<F>(lookup: &F) -> Result<(usize, usize, Toggle), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let validity_days = number(
        lookup,
        BACKUP_VALIDITY_DAYS_VAR,
        DEFAULT_BACKUP_VALIDITY_DAYS,
    )?;
    let limit_days = number(lookup, BACKUP_LIMIT_DAYS_VAR, DEFAULT_BACKUP_LIMIT_DAYS)?;
    let purge = boolean(lookup, BACKUP_PURGE_VAR, DEFAULT_BACKUP_PURGE)?.into();
    Ok((validity_days, limit_days, purge))
}

/// The reader groups that share no theme — [`upload_settings`],
/// [`misc_toggles`], [`job_worker`], [`backup_settings`] and
/// [`fetch_settings`] — flattened into one named bundle so
/// [`Config::from_lookup`] binds all thirteen of their fields with a single
/// `let` and stays inside Clippy's line budget, the same move [`Groups`]
/// spells for the wording and signup trios. The field names mirror the
/// [`Config`] fields they hang on.
#[derive(Debug)]
pub(crate) struct RestSettings {
    /// Per-request limits for multipart uploads (`UPLOAD_LIMIT_BYTES` and
    /// `UPLOAD_MAX_FILES`).
    pub(crate) upload_limits: UploadLimits,
    /// Maximum avatar image size in bytes (`AVATAR_LIMIT_BYTES`).
    pub(crate) avatar_limit_bytes: usize,
    /// Maximum banner/background image size in bytes (`BANNER_LIMIT_BYTES`).
    pub(crate) banner_limit_bytes: usize,
    /// Whether a captcha provider is configured (`CAPTCHA_ENABLED`).
    pub(crate) captcha_enabled: Toggle,
    /// Whether the healthcheck endpoint reports pool data
    /// (`HEALTHCHECK_ENABLED`).
    pub(crate) healthcheck_enabled: Toggle,
    /// The job worker's concurrent handler count (`JOB_CONCURRENCY`).
    pub(crate) job_concurrency: usize,
    /// The job queue names the worker drains (`JOB_QUEUES`).
    pub(crate) job_queues: Vec<String>,
    /// The deleted-status purge age (`PURGE_DELETED_STATUSES_DAYS`).
    pub(crate) purge_deleted_statuses_days: usize,
    /// The days a backup archive stays downloadable (`BACKUP_VALIDITY_DAYS`).
    pub(crate) backup_validity_days: usize,
    /// The minimum days between two backups of one user
    /// (`BACKUP_LIMIT_DAYS`).
    pub(crate) backup_limit_days: usize,
    /// Whether the backup purge sweep runs at all (`BACKUP_PURGE`).
    pub(crate) backup_purge: Toggle,
    /// The settings that govern how this instance fetches and is fetched by
    /// peers, from [`fetch_settings`].
    pub(crate) fetch: FetchSettings,
}

/// Reads the five groups [`RestSettings`] bundles.
///
/// # Errors
///
/// Those of [`upload_settings`], [`misc_toggles`], [`job_worker`],
/// [`backup_settings`] and [`fetch_settings`].
pub(crate) fn rest_settings<F>(lookup: &F) -> Result<RestSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let (upload_limits, avatar_limit_bytes, banner_limit_bytes) = upload_settings(lookup)?;
    let (captcha_enabled, healthcheck_enabled) = misc_toggles(lookup)?;
    let (job_concurrency, job_queues, purge_deleted_statuses_days) = job_worker(lookup)?;
    let (backup_validity_days, backup_limit_days, backup_purge) = backup_settings(lookup)?;
    let fetch = fetch_settings(lookup)?;
    Ok(RestSettings {
        upload_limits,
        avatar_limit_bytes,
        banner_limit_bytes,
        captcha_enabled,
        healthcheck_enabled,
        job_concurrency,
        job_queues,
        purge_deleted_statuses_days,
        backup_validity_days,
        backup_limit_days,
        backup_purge,
        fetch,
    })
}

/// Reads the one setting that shapes the streaming WebSocket: how many
/// seconds pass between two heartbeat pings of one socket
/// (`STREAMING_PING_INTERVAL_SECS`, 30 — Pleroma's `@tick` — when unset).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when the variable holds a value
/// that is not a whole number.
pub(crate) fn streaming_heartbeat<F>(lookup: &F) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    number(
        lookup,
        STREAMING_PING_INTERVAL_SECS_VAR,
        DEFAULT_STREAMING_PING_INTERVAL_SECS,
    )
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    #[test]
    fn job_worker_settings_default() {
        let config = config_with(&[]);
        assert_eq!(config.job_concurrency, 10, "default job_concurrency");
        assert_eq!(
            config.job_queues,
            vec![
                "default".to_owned(),
                "federator_incoming".to_owned(),
                "federator_outgoing".to_owned(),
                "mailer".to_owned(),
                "scheduled".to_owned(),
                "background".to_owned(),
            ],
            "default job_queues"
        );
        assert_eq!(
            config.purge_deleted_statuses_days, 30,
            "default PURGE_DELETED_STATUSES_DAYS is thirty days"
        );
    }

    /// The deleted-status purge age: thirty days by default, the environment
    /// overriding it, `0` expressible (the handler's disable value), a blank
    /// falling back, junk refused, and `Debug` showing the value.
    #[test]
    fn purge_deleted_statuses_days_defaults_and_reads_its_variable() {
        let config = config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "7")]);
        assert_eq!(
            config.purge_deleted_statuses_days, 7,
            "custom PURGE_DELETED_STATUSES_DAYS"
        );
        let disabled = config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "0")]);
        assert_eq!(
            disabled.purge_deleted_statuses_days, 0,
            "zero is a legal value, and the purge handler reads it as disabled"
        );
        let blank = config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "")]);
        assert_eq!(
            blank.purge_deleted_statuses_days, 30,
            "an empty value applies the default"
        );
        let err = try_config_with(&[(PURGE_DELETED_STATUSES_DAYS_VAR, "many")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidNumber(PURGE_DELETED_STATUSES_DAYS_VAR),
            "a value that is not a whole number is refused"
        );
        assert!(err.source().is_none(), "no source");
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {PURGE_DELETED_STATUSES_DAYS_VAR}: ")),
            "message names the variable"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("purge_deleted_statuses_days: 7"),
            "the purge age is visible in Debug output: {debug}"
        );
    }

    /// The backup trio: thirty valid days, a seven-day interval and the
    /// purge sweep on by default, each environment variable overriding, a
    /// blank falling back, junk in the two day settings refused, and
    /// `Debug` showing all three.
    #[test]
    fn backup_settings_default_and_read_their_variables() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.backup_validity_days, 30,
            "default BACKUP_VALIDITY_DAYS is thirty days"
        );
        assert_eq!(
            defaults.backup_limit_days, 7,
            "default BACKUP_LIMIT_DAYS is seven days"
        );
        assert!(
            defaults.backup_purge.get(),
            "the backup purge sweep runs by default"
        );
        let config = config_with(&[
            (BACKUP_VALIDITY_DAYS_VAR, "10"),
            (BACKUP_LIMIT_DAYS_VAR, "2"),
            (BACKUP_PURGE_VAR, "false"),
        ]);
        assert_eq!(
            config.backup_validity_days, 10,
            "custom BACKUP_VALIDITY_DAYS"
        );
        assert_eq!(config.backup_limit_days, 2, "custom BACKUP_LIMIT_DAYS");
        assert!(
            !config.backup_purge.get(),
            "`BACKUP_PURGE=false` stops the sweep"
        );
        let blank = config_with(&[
            (BACKUP_VALIDITY_DAYS_VAR, ""),
            (BACKUP_LIMIT_DAYS_VAR, ""),
            (BACKUP_PURGE_VAR, ""),
        ]);
        assert_eq!(
            (
                blank.backup_validity_days,
                blank.backup_limit_days,
                blank.backup_purge.get(),
            ),
            (30, 7, true),
            "an empty value applies the default"
        );
        for (var, value) in [
            (BACKUP_VALIDITY_DAYS_VAR, "many"),
            (BACKUP_LIMIT_DAYS_VAR, "weekly"),
        ] {
            let err = try_config_with(&[(var, value)]).unwrap_err();
            assert_eq!(
                err,
                ConfigError::InvalidNumber(var),
                "`{var}={value:?}` is not a whole number"
            );
            assert!(err.source().is_none(), "no source");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names the variable"
            );
        }
        let err = try_config_with(&[(BACKUP_PURGE_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(BACKUP_PURGE_VAR),
            "a value that is neither boolean spelling is refused"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("backup_validity_days: 10"),
            "the archive validity window is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("backup_limit_days: 2"),
            "the per-user backup interval is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("backup_purge: Toggle(false)"),
            "the purge switch is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_job_worker_settings() {
        let config = config_with(&[
            (JOB_CONCURRENCY_VAR, "3"),
            (JOB_QUEUES_VAR, " default , mailer ,,scheduled"),
        ]);
        assert_eq!(config.job_concurrency, 3, "custom job_concurrency");
        assert_eq!(
            config.job_queues,
            vec![
                "default".to_owned(),
                "mailer".to_owned(),
                "scheduled".to_owned(),
            ],
            "comma list is trimmed, empty entries dropped"
        );
    }

    #[test]
    fn streaming_heartbeat_defaults_and_reads_its_variable() {
        assert_eq!(
            config_with(&[]).streaming_ping_interval_secs,
            DEFAULT_STREAMING_PING_INTERVAL_SECS,
            "default streaming heartbeat"
        );
        assert_eq!(
            config_with(&[(STREAMING_PING_INTERVAL_SECS_VAR, "5")]).streaming_ping_interval_secs,
            5,
            "custom streaming heartbeat"
        );
        assert_eq!(
            config_with(&[(STREAMING_PING_INTERVAL_SECS_VAR, "")]).streaming_ping_interval_secs,
            DEFAULT_STREAMING_PING_INTERVAL_SECS,
            "an empty value applies the default"
        );
    }
}
