//! The admin config store endpoints: `GET` and `POST
//! /api/v1/pleroma/admin/config`, the HTTP face of
//! [`crate::services::runtime_config`].
//!
//! Beside the store answer the documentation payload
//! `GET /config/descriptions` and the restart pair `GET /need_reboot`
//! and `GET /restart` (WBE-207), generated from the same registry and
//! the same module's process-wide flags.
//!
//! The reference is Pleroma's
//! `lib/pleroma/web/admin_api/controllers/config_controller.ex` with the
//! value encoding of `lib/pleroma/config/transfer_task.ex`: only the
//! pairs [`config_registry`] whitelists are readable and changeable here,
//! keyword lists travel as arrays of `{"tuple": [key, value]}` entries,
//! and every stored row is an overlay on the environment-derived
//! [`Config`] rather than a replacement of it.
//!
//! - `GET /config` (`show`) — the store as Pleroma's `show` renders it:
//!   with `only_db=true` the rows exactly as stored, and otherwise every
//!   whitelisted key with its effective value (the row merged over the
//!   env default); each entry carries `db` — the subkeys the row holds —
//!   while a row exists, and `need_reboot` says whether any stored key
//!   needs a restart to take effect.
//! - `POST /config` (`update`) — upserts wire-form values (merging
//!   subkeys into the stored keyword list, Pleroma's
//!   `ConfigDB.update_or_create`) or deletes entries / subkeys; the
//!   answer is the GET shape for the changed entries, `need_reboot`
//!   true when any changed key has [`RegistryEntry::requires_restart`],
//!   such a change also raising the process-wide flag `GET /need_reboot`
//!   answers with, and every changed request records one `config_change`
//!   [`moderation_log`] entry. A pair outside the whitelist is the 400
//!   `{"error":"Config not allowed: <group>/<key>"}`; the whole request
//!   is refused before anything is written.
//! - `GET /config/descriptions` (`descriptions`) — one description
//!   object per whitelist entry — `group`, `key`, `type`, `description`
//!   and the `children` metadata (with their `suggestions`) the
//!   [`config_registry`] carries (WBE-207).
//! - `GET /need_reboot` (`need_reboot`) — `{"need_reboot": bool}`: the
//!   process-wide "changed since this boot" flag, raised by `POST
//!   /config` for a restart-requiring key and lowered only by the next
//!   boot.
//! - `GET /restart` (`restart`) — requests the graceful restart: raises
//!   the shutdown flag [`serve`](crate::serve) waits on beside ctrl-c,
//!   so the process drains and exits `0` for the supervisor; answers
//!   `{}` and leaves `need_reboot` standing.
//!
//! Pleroma answers 400 "You must enable `configurable_from_database`" on
//! both routes until the operator flips that setting; wired-be's store is
//! always on, so that refusal does not exist here. The gate that does:
//! [`AdminUser`] plus `admin:read:config` / `admin:write:config`, the
//! scope areas the ticket pins.
//!
//! [`RegistryEntry::requires_restart`]: crate::services::config_registry::RegistryEntry::requires_restart
//! [`moderation_log`]: crate::models::moderation_log

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::debug;

use crate::{
    auth::admin::AdminUser,
    config::Config,
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::moderation_log::{self, ConfigChange},
    services::{
        config_registry::{self, EntryDescription, GROUP_PLEROMA},
        runtime_config::{self, ConfigEntry, DeleteOutcome, RuntimeConfig},
    },
    state::AppState,
    util::params::truthy,
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in
/// (`admin:read:config` / `admin:write:config`).
const AREA_CONFIG: &str = "config";
/// The query parameter that switches `GET /config` to the rows-as-stored
/// answer (Pleroma's `only_db`).
const ONLY_DB: &str = "only_db";
/// The group-key prefix of the dedicated per-policy MRF entries
/// (`:mrf_simple` and friends, beside the whole-`mrf`-doc `:mrf`).
const MRF_PREFIX: &str = ":mrf";

/// The `/config`, `/config/descriptions`, `/need_reboot` and `/restart`
/// routes of the admin API, merged into [`super`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/config", get(show).post(update))
        .route("/config/descriptions", get(descriptions))
        .route("/need_reboot", get(need_reboot))
        .route("/restart", get(restart))
}

/// The one-field body of `GET /need_reboot` (Pleroma's render).
#[derive(Debug, Serialize)]
struct NeedReboot {
    /// Whether a restart-requiring key was changed since this process
    /// started.
    need_reboot: bool,
}

/// One config entry as the API renders it: the colon-spelled pair, the
/// value in wire form, and the stored `db` subkeys — absent when nothing
/// is stored (Pleroma's `put_if_present`).
#[derive(Debug, Serialize)]
struct ConfigEntryView {
    /// The configuration group, colon-spelled.
    group: String,
    /// The configuration key, colon-spelled.
    key: String,
    /// The value in wire form: keyword lists as `{"tuple": […]}` arrays.
    value: Value,
    /// The subkeys (or the plain entry's own key) the database row holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    db: Option<Vec<String>>,
}

/// The answer of both routes: Pleroma's `index.json` render.
#[derive(Debug, Serialize)]
struct ConfigPage {
    /// The entries: table or registry order for `GET`, request order for
    /// the changed entries of `POST`.
    configs: Vec<ConfigEntryView>,
    /// Whether any stored (or, after a `POST`, changed) entry needs a
    /// restart to take effect.
    need_reboot: bool,
}

/// The body of `POST /config`.
#[derive(Debug, Deserialize)]
struct UpdateBody {
    /// The entries to upsert or delete, applied in order.
    configs: Vec<UpdateItem>,
}

/// One `configs` item of the POST body: Pleroma's spex casts the same
/// shape, and `value` is required only when the item does not delete.
#[derive(Debug, Deserialize)]
struct UpdateItem {
    /// The configuration group, colon-spelled.
    group: String,
    /// The configuration key, colon-spelled.
    key: String,
    /// The wire-form value to upsert (merged into the stored row).
    #[serde(default)]
    value: Option<Value>,
    /// Delete the row (or its `subkeys`) instead of upserting.
    #[serde(default, deserialize_with = "flex::bool")]
    delete: bool,
    /// With `delete`, only these subkeys come out of the stored row.
    #[serde(default)]
    subkeys: Option<Vec<String>>,
}

/// What one POST item ended in. `changed` is whether the store differs
/// from how it was; the value arms carry what the row now holds, in
/// storage form, for the response render.
#[derive(Debug)]
enum Outcome {
    /// The upsert wrote this merged value.
    Upserted(Value),
    /// The subkey delete left this value in the row.
    PartiallyDeleted(Value),
    /// The row is gone.
    Deleted,
    /// Nothing was there to delete.
    Untouched,
}

impl Outcome {
    /// Whether this item changed the store.
    #[must_use]
    const fn changed(&self) -> bool {
        !matches!(self, Self::Untouched)
    }

    /// What the row now holds, for the response render: `None` for the
    /// whole deletes Pleroma keeps out of the updated list.
    const fn standing(&self) -> Option<&Value> {
        match self {
            Self::Upserted(value) | Self::PartiallyDeleted(value) => Some(value),
            Self::Deleted | Self::Untouched => None,
        }
    }

    /// Whether this item deleted rather than wrote, for the log.
    #[must_use]
    const fn is_delete(&self) -> bool {
        matches!(self, Self::Deleted | Self::PartiallyDeleted(_))
    }
}

/// `GET /api/v1/pleroma/admin/config` — the store as Pleroma's `show`
/// answers it: the rows with `only_db`, every whitelisted key with its
/// effective value otherwise, `need_reboot` beside them.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the store read.
async fn show(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<ConfigPage>> {
    admin.require(CLASS_READ, AREA_CONFIG)?;
    let rows = runtime_config::all(&state.db).await?;
    let configs = if is_only_db(&params) {
        rows.iter().map(view_of_row).collect()
    } else {
        effective_views(&state.config, &rows)
    };
    Ok(Json(ConfigPage {
        configs,
        need_reboot: rows_need_reboot(&rows),
    }))
}

/// `POST /api/v1/pleroma/admin/config` — Pleroma's `update`: refuse any
/// non-whitelisted pair first, then apply each entry in order, clear the
/// MRF configuration cache when a `:mrf*` row changed so the pipeline
/// reads the new chain immediately, record one `config_change` log line,
/// and answer the GET shape for the changed entries.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] when an entry is
/// not whitelisted; [`AppError::Unprocessable`] when an upsert carries no
/// `value`; [`AppError::Database`] from the store writes and the log.
async fn update(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<UpdateBody>,
) -> AppResult<Json<ConfigPage>> {
    admin.require(CLASS_WRITE, AREA_CONFIG)?;
    validate(&body.configs)?;
    let store = RuntimeConfig::new(&state.db, &state.config);
    let mut applied = Applied::default();
    for item in &body.configs {
        let outcome = apply_item(&store, item).await?;
        applied.record(item, &outcome);
    }
    if applied.mrf_changed {
        state.mrf_config_cache.clear();
    }
    if applied.need_reboot {
        // The process-wide flag `GET /need_reboot` answers with: a
        // restart-requiring key changed since this boot, and the next
        // `GET /restart` (or a signal) is what takes effect.
        RuntimeConfig::set_need_reboot();
    }
    if !applied.changes.is_empty() {
        let logged = moderation_log::config_change(&state.db, &admin.0, &applied.changes).await?;
        debug!(
            id = %logged.id,
            entries = applied.changes.len(),
            "admin config change recorded"
        );
    }
    Ok(Json(ConfigPage {
        configs: applied.configs,
        need_reboot: applied.need_reboot,
    }))
}

/// `GET /api/v1/pleroma/admin/config/descriptions` — Pleroma's
/// `descriptions`: one documentation object per whitelist entry, in
/// registry order, each with its `type`, `description` and `children`.
/// The payload is the [`config_registry`] data itself, so every pair the
/// store accepts is a pair the payload describes.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate.
async fn descriptions(admin: AdminUser) -> AppResult<Json<Vec<EntryDescription>>> {
    admin.require(CLASS_READ, AREA_CONFIG)?;
    Ok(Json(config_registry::descriptions()))
}

/// `GET /api/v1/pleroma/admin/need_reboot` — Pleroma's `need_reboot`:
/// whether a restart-requiring key changed since this process started,
/// read off the process-wide flag `POST /config` raises. Beside it, the
/// `need_reboot` field of the config pages answers what still *waits* in
/// the store; this answers what *changed* since the boot — and only the
/// next boot lowers it again.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate.
async fn need_reboot(admin: AdminUser) -> AppResult<Json<NeedReboot>> {
    admin.require(CLASS_READ, AREA_CONFIG)?;
    Ok(Json(NeedReboot {
        need_reboot: RuntimeConfig::need_reboot(),
    }))
}

/// `GET /api/v1/pleroma/admin/restart` — Pleroma's `restart`: request
/// the graceful restart. The answer is `{}` and the effect is the
/// process-wide shutdown flag [`RuntimeConfig::wait_for_restart`] carries
/// into [`serve`](crate::serve): the server drains, the process exits
/// with code `0`, and the supervisor starts the boot every pending
/// change needs. `need_reboot` is deliberately *not* cleared here — the
/// running process did not apply anything, the next one will.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate.
async fn restart(admin: AdminUser) -> AppResult<Json<Map<String, Value>>> {
    admin.require(CLASS_WRITE, AREA_CONFIG)?;
    RuntimeConfig::request_restart();
    Ok(Json(Map::new()))
}

/// Every stored row against the registry: the whole-table answer of
/// whether a change is still waiting for a restart.
fn rows_need_reboot(rows: &[ConfigEntry]) -> bool {
    rows.iter()
        .any(|row| config_registry::requires_restart(&row.group_name, &row.key))
}

/// Whether the request asked for the rows-as-stored answer. Pleroma's
/// spex casts a real boolean; the query string is text, so the spellings
/// [`truthy`] reads as true (`true`, `1`, `yes`, … in any case) read as
/// yes and anything else as no.
fn is_only_db(params: &HashMap<String, String>) -> bool {
    params
        .get(ONLY_DB)
        .is_some_and(|text| truthy(text) == Some(true))
}

/// One stored row in the API's render: the value as stored, in wire
/// form, with `db` naming what the row holds.
fn view_of_row(row: &ConfigEntry) -> ConfigEntryView {
    ConfigEntryView {
        group: row.group_name.clone(),
        key: row.key.clone(),
        value: runtime_config::to_wire(&row.value),
        db: Some(runtime_config::db_keys(&row.key, &row.value)),
    }
}

/// The GET-without-`only_db` answer: every registry entry with its
/// effective value — the row merged over the env-derived default — and
/// the row's subkeys beside it, while the store holds one.
fn effective_views(config: &Config, rows: &[ConfigEntry]) -> Vec<ConfigEntryView> {
    config_registry::entries()
        .iter()
        .map(|entry| {
            let stored = stored_of(rows, entry.group, entry.key);
            let value = runtime_config::effective_value(&(entry.defaults)(config), stored);
            ConfigEntryView {
                group: entry.group.to_owned(),
                key: entry.key.to_owned(),
                value: runtime_config::to_wire(&value),
                db: stored.map(|stored| runtime_config::db_keys(entry.key, stored)),
            }
        })
        .collect()
}

/// The stored value of one entry within a whole-table read.
fn stored_of<'r>(rows: &'r [ConfigEntry], group: &str, key: &str) -> Option<&'r Value> {
    rows.iter()
        .find(|row| row.group_name == group && row.key == key)
        .map(|row| &row.value)
}

/// The refuse-before-anything-writes checks: every pair must be on the
/// whitelist (Pleroma filters those out silently; this endpoint answers
/// the 400 the ticket pins), and a non-delete item must carry a value.
fn validate(items: &[UpdateItem]) -> AppResult<()> {
    for item in items {
        if config_registry::find(&item.group, &item.key).is_none() {
            return Err(AppError::BadRequest(format!(
                "Config not allowed: {}/{}",
                item.group, item.key
            )));
        }
        if !item.delete && item.value.is_none() {
            return Err(AppError::Unprocessable(
                "configs entries need a value unless delete is set".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Runs one validated item against the store.
async fn apply_item(store: &RuntimeConfig<'_>, item: &UpdateItem) -> AppResult<Outcome> {
    if item.delete {
        let outcome = store
            .delete(&item.group, &item.key, item.subkeys.as_deref())
            .await?;
        return Ok(match outcome {
            DeleteOutcome::Updated(remaining) => Outcome::PartiallyDeleted(remaining),
            DeleteOutcome::Removed => Outcome::Deleted,
            DeleteOutcome::Absent => Outcome::Untouched,
        });
    }
    let value = item
        .value
        .as_ref()
        .ok_or_else(|| AppError::Unprocessable("configs entries need a value".to_owned()))?;
    let merged = store.set(&item.group, &item.key, value).await?;
    Ok(Outcome::Upserted(merged))
}

/// What the POST loop has accumulated: the entries to render, the pairs
/// for the log, and the two flags the changed keys decide.
#[derive(Debug, Default)]
struct Applied<'a> {
    /// The changed entries whose row still stands (Pleroma's `updated`).
    configs: Vec<ConfigEntryView>,
    /// Every pair that actually changed, for the `config_change` record.
    changes: Vec<ConfigChange<'a>>,
    /// Whether any changed key needs a restart.
    need_reboot: bool,
    /// Whether a `:mrf*` key changed and the chain cache must go.
    mrf_changed: bool,
}

impl<'a> Applied<'a> {
    /// Folds one item in: unchanged items decide nothing, every changed
    /// pair lands in the log, a restart-flagged key raises `need_reboot`,
    /// a `:mrf*` key raises the cache clear, and a value that stands in a
    /// row renders in the response — Pleroma's updated/deleted split.
    fn record(&mut self, item: &'a UpdateItem, outcome: &Outcome) {
        if !outcome.changed() {
            return;
        }
        if config_registry::requires_restart(&item.group, &item.key) {
            self.need_reboot = true;
        }
        if item.group == GROUP_PLEROMA && item.key.starts_with(MRF_PREFIX) {
            self.mrf_changed = true;
        }
        self.changes.push(ConfigChange {
            group: &item.group,
            key: &item.key,
            delete: outcome.is_delete(),
        });
        if let Some(stored) = outcome.standing() {
            self.configs.push(ConfigEntryView {
                group: item.group.clone(),
                key: item.key.clone(),
                value: runtime_config::to_wire(stored),
                db: Some(runtime_config::db_keys(&item.key, stored)),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::services::config_registry::{KEY_INSTANCE, KEY_MRF};

    /// The whitelist refusal names the pair the client sent, and only a
    /// non-delete item without a value trips the second check.
    #[test]
    fn unknown_pairs_are_the_named_400() {
        let unknown_key = validate(&[item(GROUP_PLEROMA, ":nope", None, false)])
            .expect_err("not on the whitelist");
        assert_eq!(
            unknown_key.to_string(),
            "Config not allowed: :pleroma/:nope",
            "Pleroma's wording, the sent pair named"
        );
        let unknown_group = validate(&[item(":nope", KEY_INSTANCE, None, false)])
            .expect_err("wrong group is no pair");
        assert_eq!(
            unknown_group.to_string(),
            "Config not allowed: :nope/:instance"
        );
        let deletes = validate(&[item(GROUP_PLEROMA, KEY_MRF, None, true)]);
        assert!(deletes.is_ok(), "a delete needs no value");
        let upserts = validate(&[item(GROUP_PLEROMA, KEY_MRF, Some(json!([])), false)]);
        assert!(upserts.is_ok(), "an upsert with a value passes");
        let valueless = validate(&[item(GROUP_PLEROMA, KEY_MRF, None, false)]);
        assert!(valueless.is_err(), "an upsert without a value is no write");
    }

    /// A POST body parses: wire values, the delete flag in both the JSON
    /// boolean and the form string spelling, and absent optionals.
    #[test]
    fn the_body_parses_the_ticket_shape() {
        let body: UpdateBody = serde_json::from_value(json!({
            "configs": [
                {"group": ":pleroma", "key": ":mrf",
                 "value": [{"tuple": ["policies", ["simple", "keyword"]]}]},
                {"group": ":pleroma", "key": ":instance", "delete": true,
                 "subkeys": ["name"]},
            ],
        }))
        .expect("the ticket's body shape deserializes");
        assert_eq!(body.configs.len(), 2, "both items parse");
        assert_eq!(
            body.configs[0].value,
            Some(json!([{"tuple": ["policies", ["simple", "keyword"]]}])),
            "the wire value arrives untouched for the store to decode"
        );
        assert!(!body.configs[0].delete, "absent delete reads false");
        assert!(body.configs[1].delete, "the boolean flag parses");
        assert_eq!(
            body.configs[1].subkeys,
            Some(vec!["name".to_owned()]),
            "and so do the subkeys"
        );
        let form: UpdateBody = serde_json::from_value(json!({
            "configs": [{"group": ":pleroma", "key": ":instance", "delete": "true"}],
        }))
        .expect("the stringly-typed delete reads through flex");
        assert!(form.configs[0].delete, "form spellings included");
    }

    /// `only_db` reads the boolean spellings a query string can carry.
    #[test]
    fn only_db_reads_true_and_one() {
        let mut params = HashMap::new();
        assert!(!is_only_db(&params), "absent reads false");
        for (value, expected) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("yes", true),
            ("0", false),
            ("", false),
            ("no", false),
            ("junk", false),
        ] {
            let overwrote = params.insert(ONLY_DB.to_owned(), value.to_owned());
            drop(overwrote);
            assert_eq!(is_only_db(&params), expected, "{value} reads {expected}");
        }
    }

    /// The wire form of a stored keyword row is the tuple array the
    /// ticket's example spells, with `db` naming the stored subkeys.
    #[test]
    fn rows_render_as_tuple_arrays_with_their_db_keys() {
        let view = view_of_row(&row(KEY_INSTANCE, json!({"name": "Wired"})));
        let rendered = serde_json::to_value(&view).expect("the view serializes");
        assert_eq!(
            rendered,
            json!({
                "group": ":pleroma",
                "key": ":instance",
                "value": [{"tuple": ["name", "Wired"]}],
                "db": ["name"],
            }),
            "the ticket's shape"
        );
    }

    /// The effective GET view merges the row over the env defaults, names
    /// only the stored subkeys in `db`, and omits `db` where no row
    /// stands.
    #[test]
    fn effective_views_merge_the_defaults() {
        let config = Config::for_tests();
        let rows = vec![row(KEY_INSTANCE, json!({"name": "Overridden"}))];
        let views = effective_views(&config, &rows);
        assert_eq!(
            views.len(),
            config_registry::entries().len(),
            "every registry key answers"
        );
        let instance = serde_json::to_value(&views[0]).expect("the view serializes");
        let Value::Array(entries) = instance["value"].clone() else {
            panic!("the keyword value renders as its tuple array");
        };
        let pairs: Vec<&Vec<Value>> = entries
            .iter()
            .filter_map(|entry| entry["tuple"].as_array())
            .collect();
        let named = |key: &str, want: Value| {
            pairs
                .iter()
                .any(|pair| pair.first() == Some(&json!(key)) && pair.get(1) == Some(&want))
        };
        assert!(named("name", json!("Overridden")), "the row's subkey wins");
        assert!(
            named("description", json!(config.instance_description)),
            "and an untouched sibling keeps the env value"
        );
        assert_eq!(
            instance["db"],
            json!(["name"]),
            "db names exactly the stored subkey"
        );
        let mrf = serde_json::to_value(&views[1]).expect("the view serializes");
        assert_eq!(
            mrf.get("db"),
            None,
            "an entry with no row renders without db"
        );
        assert_eq!(
            mrf["value"],
            json!([{"tuple": ["policies", []]}]),
            "and the :mrf value is the env chain"
        );
    }

    /// `need_reboot` reads the stored rows against the registry flags:
    /// no whitelisted key waits for a restart, and a stale row of a key
    /// that is no longer whitelisted (`:media_proxy`, accepted before the
    /// key was dropped) raises nothing either.
    #[test]
    fn need_reboot_follows_the_stored_keys() {
        assert!(!rows_need_reboot(&[]), "an empty store reboots nobody");
        assert!(
            !rows_need_reboot(&[row(KEY_INSTANCE, json!({"name": "Wired"}))]),
            ":instance takes effect live"
        );
        assert!(
            !rows_need_reboot(&[
                row(KEY_INSTANCE, json!({"name": "Wired"})),
                row(":media_proxy", json!({"enabled": false})),
            ]),
            "a stale row of a dropped key restarts nothing"
        );
    }

    /// The two keys no runtime consumer reads are refused as not
    /// whitelisted, the same 400 as any unknown pair.
    #[test]
    fn unread_keys_are_refused_as_not_whitelisted() {
        for key in [":restrict_unauthenticated", ":media_proxy"] {
            let refused = validate(&[item(GROUP_PLEROMA, key, Some(json!({})), false)])
                .expect_err("the key is not whitelisted");
            assert_eq!(
                refused.to_string(),
                format!("Config not allowed: :pleroma/{key}"),
                "{key} answers the not-whitelisted 400"
            );
        }
    }

    /// The accumulator: only changed items count, upserted entries
    /// render, whole deletes only land in the log and the flag, and the
    /// `:mrf*` family flips the cache-clear marker.
    #[test]
    fn the_accumulator_folds_changed_items() {
        let mrf = item(GROUP_PLEROMA, KEY_MRF, Some(json!([])), false);
        let mut applied = Applied::default();
        applied.record(&mrf, &Outcome::Upserted(json!({"policies": ["simple"]})));
        applied.record(&mrf, &Outcome::Untouched);
        assert_eq!(applied.configs.len(), 1, "the upsert renders once");
        assert_eq!(applied.changes.len(), 1, "and logs once");
        assert!(
            applied.mrf_changed,
            "the :mrf family invalidates the chain cache"
        );
        assert!(!applied.need_reboot, ":mrf needs no restart");
        let view = serde_json::to_value(&applied.configs[0]).expect("the view serializes");
        assert_eq!(
            view["value"],
            json!([{"tuple": ["policies", ["simple"]]}]),
            "the stored value travels in wire form"
        );
        assert_eq!(view["db"], json!(["policies"]), "with its stored subkeys");

        let deleted = item(GROUP_PLEROMA, KEY_INSTANCE, None, true);
        applied.record(&deleted, &Outcome::Deleted);
        assert!(!applied.need_reboot, "no whitelisted key is restart-gated");
        assert_eq!(
            applied.configs.len(),
            1,
            "and a whole delete renders nothing"
        );
        assert_eq!(applied.changes.len(), 2, "but logs");
        assert!(applied.changes[1].delete, "as a delete");
        assert!(!applied.changes[0].delete, "beside the upsert");

        let instance = item(GROUP_PLEROMA, KEY_INSTANCE, None, true);
        applied.record(
            &instance,
            &Outcome::PartiallyDeleted(json!({"email": "a@b.c"})),
        );
        assert_eq!(
            applied.configs.len(),
            2,
            "a subkey delete leaves a row to render"
        );
    }

    /// A POST item in the ticket's shape.
    fn item(group: &str, key: &str, value: Option<Value>, delete: bool) -> UpdateItem {
        UpdateItem {
            group: group.to_owned(),
            key: key.to_owned(),
            value,
            delete,
            subkeys: None,
        }
    }

    /// The reboot pair's bodies: `need_reboot` renders Pleroma's
    /// one-field answer, and `restart` the empty object.
    #[test]
    fn the_reboot_pair_renders_the_ticket_bodies() {
        let flag =
            serde_json::to_value(NeedReboot { need_reboot: true }).expect("the view serializes");
        assert_eq!(flag, json!({ "need_reboot": true }), "the flagged answer");
        let lowered =
            serde_json::to_value(NeedReboot { need_reboot: false }).expect("the view serializes");
        assert_eq!(lowered, json!({ "need_reboot": false }), "and the calm one");
        let ack =
            serde_json::to_value(Map::<String, Value>::new()).expect("the empty object serializes");
        assert_eq!(ack, json!({}), "and restart answers the empty object");
    }

    /// A stored row for the table-driven tests.
    fn row(key: &str, value: Value) -> ConfigEntry {
        ConfigEntry {
            group_name: GROUP_PLEROMA.to_owned(),
            key: key.to_owned(),
            value,
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }
}
