//! The admin runtime configuration store: the `config_entries` table,
//! the effective values it produces over the environment-derived
//! [`Config`], and the consumers that read through it.
//!
//! The reference is Pleroma's `Pleroma.ConfigDB`
//! (`lib/pleroma/config_db.ex`) beside `Pleroma.Config.TransferTask`
//! (`lib/pleroma/config/transfer_task.ex`): one row per whitelisted
//! `(group, key)` pair, the row's JSON overlaid on the boot-time value.
//! Here the whitelist is [`config_registry`], the overlay is
//! [`RuntimeConfig::get`], and — instead of Pleroma's env-rewriting
//! transfer task — the consumers read the store directly: the MRF
//! pipeline through [`RuntimeConfig::mrf_document`] and the instance
//! documents through [`RuntimeConfig::effective_config`].
//!
//! Beside the table the module also carries the two process-wide flags
//! of the admin API's reboot pair: the [`RuntimeConfig::need_reboot`]
//! `AtomicBool` a restart-requiring `POST /config` raises, and the
//! graceful-restart request [`serve`] waits on beside ctrl-c.
//!
//! Two value forms live in this module. The **storage form** is what the
//! jsonb column holds: keyword lists as JSON objects, everything else as
//! plain JSON. The **wire form** is what the admin API exchanges: an
//! object as an array of `{"tuple": [key, value]}` entries (Pleroma's
//! `to_json_types`/`to_elixir_types` pair), recursively. [`to_wire`] and
//! [`to_storage`] translate, and the two are inverses for every value
//! the API can send.
//!
//! [`config_registry`]: super::config_registry
//! [`serve`]: crate::serve

use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::watch;

#[cfg(doc)]
use crate::config::Config;

#[cfg(test)]
mod db_tests;
mod overrides;
mod storage;
mod view;
mod wire;

pub use self::{
    storage::{all, db_keys, remove, stored, write},
    view::RuntimeConfig,
    wire::{effective_value, merge_value, to_storage, to_wire},
};

/// The process-wide "a restart-requiring key changed since this boot"
/// flag [`RuntimeConfig::need_reboot`] answers with, raised by
/// [`RuntimeConfig::set_need_reboot`] — `POST /config` does that whenever
/// a changed key's [`RegistryEntry::requires_restart`] says so. Pleroma's
/// `Restarter.Pleroma.need_reboot?/0` reads the same fact off the config
/// tracker; here it is one atomic set at change time and lowered only by
/// the next boot, when everything takes effect anyway.
///
/// [`RegistryEntry::requires_restart`]: super::config_registry::RegistryEntry::requires_restart
static NEED_REBOOT: AtomicBool = AtomicBool::new(false);

/// The graceful-restart flag's channel: the sender that raises the flag
/// beside a receiver every waiter clones to observe it.
type RestartSignal = (watch::Sender<bool>, watch::Receiver<bool>);

/// The process-wide graceful-restart request: the shutdown flag
/// [`RuntimeConfig::request_restart`] raises and [`serve`] observes
/// through [`RuntimeConfig::wait_for_restart`], beside the wakeup half
/// that turns the flag into a future. The channel lives in a static
/// because the requester is an HTTP handler and the waiter the boot
/// path — no request state connects them.
///
/// [`serve`]: crate::serve
static RESTART: OnceLock<RestartSignal> = OnceLock::new();

/// The restart flag's channel, minted on first use; the static keeps the
/// sender alive, so [`watch::Receiver::changed`] never loses a request.
fn restart_channel() -> &'static RestartSignal {
    RESTART.get_or_init(|| watch::channel(false))
}

/// One `config_entries` row: a whitelisted setting's DB-side value.
#[derive(Clone, Debug, Eq, PartialEq, sqlx::FromRow)]
pub struct ConfigEntry {
    /// The configuration group, colon-spelled (`:pleroma`). The column is
    /// `group_name` because `group` is reserved in PostgreSQL.
    pub group_name: String,
    /// The configuration key, colon-spelled (`:instance`, `:mrf`, …).
    pub key: String,
    /// The value in storage form (see the [module docs](self)).
    pub value: Value,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row was last written.
    pub updated_at: DateTime<Utc>,
}

/// What a [`RuntimeConfig::delete`] call ended in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeleteOutcome {
    /// No row was there to delete: nothing changed.
    Absent,
    /// The whole row is gone — asked for, or the consequence of dropping
    /// its last subkey (Pleroma's `delete` removes the row when the
    /// remaining keyword list comes out empty).
    Removed,
    /// Only the named subkeys went, and the row keeps `remaining`.
    Updated(Value),
}
