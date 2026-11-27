//! The effective view of the runtime configuration: process flags, reads,
//! writes and the config the instance row overlays.

use std::sync::atomic::Ordering;

use serde_json::{Map, Value};
use sqlx::PgPool;

#[cfg(doc)]
use super::to_wire;
use super::{
    DeleteOutcome, NEED_REBOOT, all, decode_frontend_configurations, effective_value,
    frontend::replace_subkeys, merge_value, overrides::apply_instance_overrides, remove,
    restart_channel, stored, to_storage, wire::normalize_key, write,
};
use crate::{
    config::Config,
    error::{AppError, AppResult},
    services::config_registry::{
        self, GROUP_PLEROMA, KEY_FRONTEND_CONFIGURATIONS, KEY_INSTANCE, KEY_MRF,
    },
};

/// The store as one request sees it: table access beside the
/// environment-derived values a stored row overlays. Cheap to build —
/// two references — so every consumer mints its own.
#[derive(Debug)]
pub struct RuntimeConfig<'a> {
    /// The pool the `config_entries` table lives on.
    db: &'a PgPool,
    /// The boot-time settings the registry's defaults read from.
    env: &'a Config,
}

impl<'a> RuntimeConfig<'a> {
    /// The store over `db`'s table and `env`'s fallback values.
    #[must_use]
    pub const fn new(db: &'a PgPool, env: &'a Config) -> Self {
        Self { db, env }
    }

    /// Whether a restart-requiring key changed since this process
    /// started: the process-wide flag `GET /need_reboot` answers with,
    /// raised by [`Self::set_need_reboot`] and lowered only by the next
    /// boot (or, in tests, [`Self::reset_process_flags`]).
    #[must_use]
    pub fn need_reboot() -> bool {
        NEED_REBOOT.load(Ordering::SeqCst)
    }

    /// Raises the process-wide [`Self::need_reboot`] flag: `POST /config`
    /// calls this once any changed key waits for a restart.
    pub fn set_need_reboot() {
        NEED_REBOOT.store(true, Ordering::SeqCst);
    }

    /// Whether a graceful restart has been requested since this process
    /// started — the flag [`Self::request_restart`] raises.
    #[must_use]
    pub fn restart_requested() -> bool {
        let (_, receiver) = restart_channel();
        *receiver.clone().borrow_and_update()
    }

    /// Requests the graceful restart: raises the shutdown flag
    /// [`serve`] observes through [`Self::wait_for_restart`] and returns.
    /// The answer to `GET /restart`, which is the admin's "let the
    /// supervisor restart me" button — Pleroma's
    /// `Restarter.Pleroma.restart/2`; here the request only flags, and
    /// the process exiting with code `0` after draining is the restart.
    ///
    /// [`serve`]: crate::serve
    pub fn request_restart() {
        let (sender, _) = restart_channel();
        let sent = sender.send(true);
        debug_assert!(sent.is_ok(), "the static keeps a receiver alive");
    }

    /// Resolves once [`Self::request_restart`] has fired — the shutdown
    /// flag's future half, which [`serve`] waits on beside ctrl-c. The
    /// watch channel delivers the flag without a lost wakeup: a request
    /// that lands before this starts waiting is already in its hand.
    ///
    /// [`serve`]: crate::serve
    pub async fn wait_for_restart() {
        let (_, receiver) = restart_channel();
        let mut changes = receiver.clone();
        loop {
            if *changes.borrow_and_update() {
                return;
            }
            if changes.changed().await.is_err() {
                // The sender lives in a static, so this only happens at
                // process teardown: no restart can arrive any more.
                std::future::pending::<()>().await;
            }
        }
    }

    /// Test support: lowers the two process-wide flags, so a test can
    /// watch `need_reboot` toggle and assert on the restart request
    /// without inheriting what an earlier test in the same binary set.
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn reset_process_flags() {
        NEED_REBOOT.store(false, Ordering::SeqCst);
        let (sender, _) = restart_channel();
        let sent = sender.send(false);
        debug_assert!(sent.is_ok(), "the static keeps a receiver alive");
    }

    /// The effective value of `(group, key)`: the stored row merged over
    /// the registry's env-derived default — or, for a pair the registry
    /// does not list, the stored row alone. `None` when neither exists.
    ///
    /// # Errors
    ///
    /// Any database failure arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn get(&self, group: &str, key: &str) -> AppResult<Option<Value>> {
        let value = stored(self.db, group, key).await?;
        Ok(self.effective(group, key, value.as_ref()))
    }

    /// [`get`](Self::get)'s answer computed from a value already read —
    /// the form `GET /config` uses after its single whole-table pass.
    #[must_use]
    pub fn effective(&self, group: &str, key: &str, stored: Option<&Value>) -> Option<Value> {
        config_registry::find(group, key)
            .map(|entry| effective_value(&(entry.defaults)(self.env), stored))
            .or_else(|| stored.cloned())
    }

    /// Upserts the pair with a wire-form `value`: decode it, merge the
    /// subkeys into the stored row (Pleroma's `update_or_create`), write
    /// the result and hand it back in storage form. An empty wire array
    /// is the empty keyword list — the form [`to_wire`] gives a stored
    /// `{}` — and decodes to that object, so it merges as the no-op
    /// Pleroma's `Keyword.merge` answers.
    ///
    /// # Errors
    ///
    /// As [`stored`].
    pub async fn set(&self, group: &str, key: &str, value: &Value) -> AppResult<Value> {
        let old = stored(self.db, group, key).await?;
        let merged = if key == KEY_FRONTEND_CONFIGURATIONS {
            let incoming = decode_frontend_configurations(value).map_err(AppError::BadRequest)?;
            replace_subkeys(old.as_ref(), incoming)
        } else {
            let decoded = to_storage(value);
            let incoming = if decoded.as_array().is_some_and(Vec::is_empty) {
                Value::Object(Map::new())
            } else {
                decoded
            };
            old.as_ref().map_or_else(
                || incoming.clone(),
                |previous| merge_value(previous, &incoming),
            )
        };
        write(self.db, group, key, &merged).await?;
        Ok(merged)
    }

    /// Deletes the pair — the whole row, or, with `subkeys`, only those
    /// names out of the stored keyword list (Pleroma's `delete/1`,
    /// including its drop of the row a fully emptied list leaves).
    ///
    /// # Errors
    ///
    /// As [`stored`].
    pub async fn delete(
        &self,
        group: &str,
        key: &str,
        subkeys: Option<&[String]>,
    ) -> AppResult<DeleteOutcome> {
        let Some(old) = stored(self.db, group, key).await? else {
            return Ok(DeleteOutcome::Absent);
        };
        let Value::Object(map) = &old else {
            let _existed = remove(self.db, group, key).await?;
            return Ok(DeleteOutcome::Removed);
        };
        let Some(dropped) = subkeys else {
            let _existed = remove(self.db, group, key).await?;
            return Ok(DeleteOutcome::Removed);
        };
        let mut remaining = map.clone();
        for subkey in dropped {
            let gone = remaining.remove(&normalize_key(subkey));
            drop(gone);
        }
        if remaining.is_empty() {
            let _existed = remove(self.db, group, key).await?;
            return Ok(DeleteOutcome::Removed);
        }
        let value = Value::Object(remaining);
        write(self.db, group, key, &value).await?;
        Ok(DeleteOutcome::Updated(value))
    }

    /// The [`Config`] the instance documents should answer with: the
    /// env-derived one with the stored `:instance` overrides applied to
    /// the fields their subkeys name. A stored value of the wrong type
    /// warns and keeps the env setting — a broken row must never turn
    /// the public document into a 500.
    ///
    /// # Errors
    ///
    /// Any database failure arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn effective_config(&self) -> AppResult<Config> {
        let mut config = self.env.clone();
        let stored = stored(self.db, GROUP_PLEROMA, KEY_INSTANCE).await?;
        if let Some(Value::Object(overrides)) = stored {
            apply_instance_overrides(&mut config, &overrides);
        }
        Ok(config)
    }

    /// The MRF configuration document the pipeline reads: the stored
    /// `:mrf` entry's fields, with every `:mrf_<policy>` entry under its
    /// policy's params key — the shape `MrfConfig::resolve` always took,
    /// now sourced from `config_entries` in place of the old `mrf`
    /// `instance_settings` row. `None` while the store holds none of
    /// them, which is where the `MRF_POLICIES` fallback answers.
    ///
    /// # Errors
    ///
    /// As [`stored`].
    pub async fn mrf_document(&self) -> AppResult<Option<Value>> {
        let mut document = Map::new();
        let mut per_policy: Vec<(String, Value)> = Vec::new();
        for row in all(self.db).await? {
            if row.group_name != GROUP_PLEROMA {
                continue;
            }
            if row.key == KEY_MRF {
                document.extend(object_fields(&row.value));
            } else if let Some(policy) = row.key.strip_prefix(":mrf_") {
                per_policy.push((policy.to_owned(), row.value));
            }
        }
        for (policy, value) in per_policy {
            let previous = document.insert(policy, value);
            drop(previous);
        }
        Ok((!document.is_empty()).then_some(Value::Object(document)))
    }
}

/// The fields a stored object carries, and the empty map for any other
/// value: the `:mrf` row's contribution to the composed MRF document.
fn object_fields(value: &Value) -> Map<String, Value> {
    let Value::Object(fields) = value else {
        return Map::new();
    };
    fields.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that read the process-wide flags: they are
    /// one per process, so only one test may own them at a time. An
    /// async lock because the flag tests await between the assertions.
    static PROCESS_FLAGS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The restart pair: `set_need_reboot` raises the flag `GET
    /// /need_reboot` answers with, `request_restart` raises the shutdown
    /// flag, resolves every waiter, and leaves `need_reboot` standing —
    /// clearing it is the next boot's business, not this endpoint's.
    #[tokio::test]
    async fn the_process_flags_toggle_on_request() {
        let _guard = PROCESS_FLAGS_LOCK.lock().await;
        RuntimeConfig::reset_process_flags();
        assert!(
            !RuntimeConfig::need_reboot(),
            "a fresh boot waits for nobody"
        );
        assert!(
            !RuntimeConfig::restart_requested(),
            "and nobody has asked it to restart"
        );

        RuntimeConfig::set_need_reboot();
        assert!(
            RuntimeConfig::need_reboot(),
            "the POST /config raise sticks"
        );

        let waiter = tokio::spawn(RuntimeConfig::wait_for_restart());
        RuntimeConfig::request_restart();
        waiter.await.expect("the waiter finishes");
        assert!(
            RuntimeConfig::restart_requested(),
            "the shutdown flag stands for serve() to observe"
        );
        assert!(
            RuntimeConfig::need_reboot(),
            "requesting the restart does NOT clear need_reboot"
        );

        RuntimeConfig::reset_process_flags();
        assert!(
            !RuntimeConfig::need_reboot() && !RuntimeConfig::restart_requested(),
            "the reset is the test affordance it advertises"
        );
    }

    /// The watch channel loses no wakeup: waiting after the request
    /// already landed resolves on the spot.
    #[tokio::test]
    async fn a_late_waiter_sees_the_early_request() {
        let _guard = PROCESS_FLAGS_LOCK.lock().await;
        RuntimeConfig::reset_process_flags();
        RuntimeConfig::request_restart();
        RuntimeConfig::wait_for_restart().await;
        assert!(RuntimeConfig::restart_requested(), "the flag is the state");
        RuntimeConfig::reset_process_flags();
    }
}
