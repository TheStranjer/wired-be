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
//! Two value forms live in this module. The **storage form** is what the
//! jsonb column holds: keyword lists as JSON objects, everything else as
//! plain JSON. The **wire form** is what the admin API exchanges: an
//! object as an array of `{"tuple": [key, value]}` entries (Pleroma's
//! `to_json_types`/`to_elixir_types` pair), recursively. [`to_wire`] and
//! [`to_storage`] translate, and the two are inverses for every value
//! the API can send.
//!
//! [`config_registry`]: super::config_registry

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sqlx::PgPool;
use tracing::{debug, warn};

use crate::{
    config::{Config, Toggle},
    error::AppResult,
    services::config_registry::{self, GROUP_PLEROMA, KEY_INSTANCE, KEY_MRF},
};

/// [`stored`]: the one row's JSON value.
const STORED_SQL: &str = "SELECT value FROM config_entries WHERE group_name = $1 AND key = $2";

/// [`all`]: every row, stably ordered by the composite primary key.
const ALL_SQL: &str = "SELECT group_name, key, value, inserted_at, updated_at FROM config_entries
     ORDER BY group_name, key";

/// [`write`]: insert-or-replace, restamping `updated_at` on the way
/// through — the shape Pleroma's `update_or_create` ends in.
const WRITE_SQL: &str = "INSERT INTO config_entries (group_name, key, value)
     VALUES ($1, $2, $3)
     ON CONFLICT (group_name, key)
     DO UPDATE SET value = EXCLUDED.value, updated_at = now()";

/// [`remove`]: the whole row.
const REMOVE_SQL: &str = "DELETE FROM config_entries WHERE group_name = $1 AND key = $2";

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

/// Reads the stored value of one entry, in storage form; `None` while the
/// store holds no row for the pair.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn stored(db: &PgPool, group: &str, key: &str) -> AppResult<Option<Value>> {
    let value = sqlx::query_scalar::<_, Value>(STORED_SQL)
        .bind(group)
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(value)
}

/// Every stored row, ordered by `(group, key)`.
///
/// # Errors
///
/// As [`stored`].
pub async fn all(db: &PgPool) -> AppResult<Vec<ConfigEntry>> {
    let rows = sqlx::query_as::<_, ConfigEntry>(ALL_SQL)
        .fetch_all(db)
        .await?;
    Ok(rows)
}

/// Upserts `value` (storage form) for the pair verbatim — the merge of
/// overlapping subkeys is [`RuntimeConfig::set`]'s business.
///
/// # Errors
///
/// As [`stored`].
pub async fn write(db: &PgPool, group: &str, key: &str, value: &Value) -> AppResult<()> {
    let written = sqlx::query(WRITE_SQL)
        .bind(group)
        .bind(key)
        .bind(value)
        .execute(db)
        .await?;
    debug!(
        group,
        key,
        rows = written.rows_affected(),
        "config entry written"
    );
    Ok(())
}

/// Deletes the pair's whole row, answering whether one was there.
///
/// # Errors
///
/// As [`stored`].
pub async fn remove(db: &PgPool, group: &str, key: &str) -> AppResult<bool> {
    let gone = sqlx::query(REMOVE_SQL)
        .bind(group)
        .bind(key)
        .execute(db)
        .await?;
    Ok(gone.rows_affected() > 0)
}

/// Translates the admin API's wire form into the storage form: an array
/// of `{"tuple": [key, value]}` entries becomes the JSON object they
/// spell, and object keys lose a leading `:`.
///
/// A lone `{"tuple": …}` pair object becomes its one-entry object, and so
/// both this API's plain `"name"` and Pleroma's `":name"` subkey
/// spellings land on the same stored key. Other values pass through
/// shape-wise, translated recursively.
#[must_use]
pub fn to_storage(value: &Value) -> Value {
    match value {
        Value::Array(items) => tuples_to_object(items).map_or_else(
            || Value::Array(items.iter().map(to_storage).collect()),
            Value::Object,
        ),
        Value::Object(map) => match tuple_pair(map) {
            Some((key, inner)) => {
                let mut object = Map::new();
                let displaced = object.insert(normalize_key(key), to_storage(inner));
                drop(displaced);
                Value::Object(object)
            }
            None => Value::Object(
                map.iter()
                    .map(|(key, inner)| (normalize_key(key), to_storage(inner)))
                    .collect(),
            ),
        },
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            scalar.clone()
        }
    }
}

/// Translates the storage form into the admin API's wire form: every JSON
/// object (recursively) becomes its array of `{"tuple": [key, value]}`
/// entries — Pleroma's keyword-list encoding.
///
/// Arrays of plain values and scalars stay JSON as they are.
#[must_use]
pub fn to_wire(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Array(
            map.iter()
                .map(|(key, inner)| tuple_entry(Value::String(key.clone()), to_wire(inner)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(to_wire).collect()),
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            scalar.clone()
        }
    }
}

/// The `{"tuple": [key, value]}` entry of the wire form.
fn tuple_entry(key: Value, value: Value) -> Value {
    let mut entry = Map::new();
    let previous = entry.insert("tuple".to_owned(), Value::Array(vec![key, value]));
    drop(previous);
    Value::Object(entry)
}

/// The object an all-tuple array spells, `None` when any element is no
/// `{"tuple": [key, value]}` pair. The empty array is no keyword list
/// here either — a data list stays a list — and [`RuntimeConfig::set`]
/// is where the entry value's empty keyword form (`{}` on the storage
/// side, `[]` on the wire) gets reconciled.
fn tuples_to_object(items: &[Value]) -> Option<Map<String, Value>> {
    if items.is_empty() {
        return None;
    }
    let mut object = Map::new();
    for item in items {
        let Value::Object(map) = item else {
            return None;
        };
        let (key, value) = tuple_pair(map)?;
        let previous = object.insert(normalize_key(key), to_storage(value));
        drop(previous);
    }
    Some(object)
}

/// The `(key, value)` a `{"tuple": [key, value]}` object spells, `None`
/// for any other object. The key must be a string; a number-keyed or
/// nested-tuple key is data, not a pair.
fn tuple_pair(map: &Map<String, Value>) -> Option<(&String, &Value)> {
    if map.len() != 1 {
        return None;
    }
    let Value::Array(items) = map.get("tuple")? else {
        return None;
    };
    let [Value::String(key), value] = items.as_slice() else {
        return None;
    };
    Some((key, value))
}

/// The storage key a wire key names: the name without its leading `:`.
fn normalize_key(key: &str) -> String {
    key.strip_prefix(':').unwrap_or(key).to_owned()
}

/// `new` over `old`, the way Pleroma's `ConfigDB.merge` folds keyword
/// lists: two objects merge key by key, recursively.
///
/// Any other pair of values is the replacement Pleroma's last-clause-wins
/// `deep_merge` answers.
#[must_use]
pub fn merge_value(old: &Value, new: &Value) -> Value {
    let (Value::Object(existing), Value::Object(incoming)) = (old, new) else {
        return new.clone();
    };
    let mut merged = existing.clone();
    for (key, value) in incoming {
        let next = merged
            .get(key)
            .map_or_else(|| value.clone(), |previous| merge_value(previous, value));
        let displaced = merged.insert(key.clone(), next);
        drop(displaced);
    }
    Value::Object(merged)
}

/// The entry's effective value: the stored row over the registry
/// default, Pleroma's `ConfigDB.merge` of the boot config and the DB.
#[must_use]
pub fn effective_value(default: &Value, stored: Option<&Value>) -> Value {
    stored.map_or_else(|| default.clone(), |value| merge_value(default, value))
}

/// The `db` field of a stored value: its subkey names when it is a
/// keyword list (object), the entry's own key when it is a plain value —
/// Pleroma's `ConfigDB.get_db_keys`.
#[must_use]
pub fn db_keys(key: &str, value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            vec![key.to_owned()]
        }
    }
}

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
        let decoded = to_storage(value);
        let incoming = if decoded.as_array().is_some_and(Vec::is_empty) {
            Value::Object(Map::new())
        } else {
            decoded
        };
        let old = stored(self.db, group, key).await?;
        let merged = old.as_ref().map_or_else(
            || incoming.clone(),
            |previous| merge_value(previous, &incoming),
        );
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

/// Overlays the stored `:instance` subkeys onto `config`, one whitelisted
/// field at a time (the set [`config_registry`] enumerates).
fn apply_instance_overrides(config: &mut Config, overrides: &Map<String, Value>) {
    if let Some(name) = override_text(overrides, "name") {
        name.clone_into(&mut config.instance_name);
    }
    if let Some(description) = override_text(overrides, "description") {
        description.clone_into(&mut config.instance_description);
    }
    if let Some(email) = override_text(overrides, "email") {
        config.contact_email = Some(email.to_owned());
    }
    if let Some(open) = override_flag(overrides, "registrations_open") {
        config.registrations_open = Toggle::new(open);
    }
    if let Some(approval) = override_flag(overrides, "account_approval_required") {
        config.registrations_approval_required = Toggle::new(approval);
    }
    if let Some(activation) = override_flag(overrides, "account_activation_required") {
        config.account_activation_required = Toggle::new(activation);
    }
    if let Some(invites) = override_flag(overrides, "invites_enabled") {
        config.invites_enabled = Toggle::new(invites);
    }
    if let Some(limit) = override_count(overrides, "limit") {
        config.max_status_chars = limit;
    }
    if let Some(attachments) = override_count(overrides, "max_media_attachments") {
        config.max_media_attachments = attachments;
    }
}

/// One stored text subkey, warned away when the row carries another type.
fn override_text<'m>(overrides: &'m Map<String, Value>, key: &str) -> Option<&'m str> {
    let value = overrides.get(key)?;
    let text = value.as_str();
    if text.is_none() {
        warn!(key, value = %value, "a :instance override is not text; the env value stands");
    }
    text
}

/// One stored boolean subkey, warned away when the row carries another
/// type.
fn override_flag(overrides: &Map<String, Value>, key: &str) -> Option<bool> {
    let value = overrides.get(key)?;
    let flag = value.as_bool();
    if flag.is_none() {
        warn!(key, value = %value, "a :instance override is not a boolean; the env value stands");
    }
    flag
}

/// One stored count subkey, warned away when the row carries another
/// type or a number past `usize`'s range.
fn override_count(overrides: &Map<String, Value>, key: &str) -> Option<usize> {
    let value = overrides.get(key)?;
    let count = value
        .as_u64()
        .and_then(|number| usize::try_from(number).ok());
    if count.is_none() {
        warn!(key, value = %value, "a :instance override is not a count; the env value stands");
    }
    count
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::services::config_registry::KEY_MRF_SIMPLE;

    /// The wire form spells stored objects as tuple arrays — and plain
    /// JSON data (arrays of strings, scalars, pair lists) as itself.
    #[test]
    fn to_wire_encodes_keyword_objects_as_tuples() {
        assert_eq!(
            to_wire(&json!({"name": "Wired", "registrations_open": false})),
            json!([
                {"tuple": ["name", "Wired"]},
                {"tuple": ["registrations_open", false]},
            ]),
            "one entry per subkey, in stored order"
        );
        assert_eq!(
            to_wire(&json!({})),
            json!([]),
            "the empty keyword list is the empty array"
        );
        assert_eq!(
            to_wire(&json!({"reject": [["evil.example", "bots"]]})),
            json!([
                {"tuple": ["reject", [["evil.example", "bots"]]]}
            ]),
            "a pair LIST stays a list — only objects become tuples"
        );
        assert_eq!(to_wire(&json!("text")), json!("text"), "scalars pass");
    }

    /// [`to_storage`] decodes tuple arrays back — accepting both this
    /// API's plain subkeys and Pleroma's colon-prefixed spellings — and
    /// is the inverse of [`to_wire`].
    #[test]
    fn to_storage_decodes_tuples_and_normalizes_keys() {
        assert_eq!(
            to_storage(&json!([
                {"tuple": ["name", "Wired"]},
                {"tuple": ["limit", 5000]},
            ])),
            json!({"name": "Wired", "limit": 5000}),
            "the tuple array becomes its object"
        );
        assert_eq!(
            to_storage(&json!([{"tuple": [":name", "Wired"]}])),
            json!({"name": "Wired"}),
            "a colon-prefixed subkey stores without it"
        );
        assert_eq!(
            to_storage(&json!([{"tuple": ["timelines", [{"tuple": ["local", true]}]]}])),
            json!({"timelines": {"local": true}}),
            "nesting decodes recursively"
        );
        assert_eq!(
            to_storage(&json!(["plain", "strings"])),
            json!(["plain", "strings"]),
            "a data list is not keyword material"
        );
        assert_eq!(
            to_storage(&json!([])),
            json!([]),
            "the empty array stays the empty list it is"
        );
        let wire = json!([{"tuple": ["reject", [["a.example", "why"]]]}]);
        assert_eq!(
            to_wire(&to_storage(&wire)),
            wire,
            "wire → storage → wire round-trips"
        );
    }

    /// [`merge_value`] overlays objects key by key, recursively, and
    /// replaces anything that is not a keyword object — Pleroma's
    /// `deep_merge` outcome.
    #[test]
    fn merging_overlays_subkeys_and_replaces_scalars() {
        assert_eq!(
            merge_value(
                &json!({"name": "old", "email": "a@b.c", "nested": {"x": 1, "y": 2}}),
                &json!({"name": "new", "nested": {"y": 3}})
            ),
            json!({"name": "new", "email": "a@b.c", "nested": {"x": 1, "y": 3}}),
            "shared subkeys win, siblings stay, nesting merges deeply"
        );
        assert_eq!(
            merge_value(&json!({"a": 1}), &json!(["list"])),
            json!(["list"]),
            "a plain value replaces the keyword object"
        );
        assert_eq!(
            merge_value(&json!(7), &json!(9)),
            json!(9),
            "and replaces anything else"
        );
    }

    /// [`db_keys`] lists stored subkeys for a keyword value and the
    /// entry's own key for a plain one (Pleroma's `get_db_keys`).
    #[test]
    fn db_keys_name_the_stored_subkeys_or_the_key() {
        assert_eq!(
            db_keys(":instance", &json!({"name": "Wired", "email": ""})),
            vec!["email".to_owned(), "name".to_owned()],
            "stored subkeys, in key order"
        );
        assert_eq!(
            db_keys(":media_proxy", &json!(true)),
            vec![":media_proxy".to_owned()],
            "a plain value names its entry"
        );
    }

    /// The stored row is [`RuntimeConfig::get`]'s answer, and the
    /// registry default answers while no row exists.
    #[tokio::test]
    async fn get_reads_the_row_over_the_env_fallback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let store = RuntimeConfig::new(&pool, &config);

        let name = store
            .get(GROUP_PLEROMA, KEY_INSTANCE)
            .await
            .expect("the lookup runs")
            .expect("a registered key always has an effective value");
        assert_eq!(
            name["name"],
            json!(config.instance_name),
            "with no row the env value answers"
        );

        let written = store
            .set(
                GROUP_PLEROMA,
                KEY_INSTANCE,
                &json!([{"tuple": ["name", "Renamed"]}]),
            )
            .await
            .expect("the write runs");
        assert_eq!(
            written,
            json!({"name": "Renamed"}),
            "the upsert hands back the merged row"
        );
        let effective = store
            .get(GROUP_PLEROMA, KEY_INSTANCE)
            .await
            .expect("the lookup runs")
            .expect("registered");
        assert_eq!(effective["name"], json!("Renamed"), "the row wins");
        assert_eq!(
            effective["description"],
            json!(config.instance_description),
            "and the untouched siblings keep the env value"
        );

        assert_eq!(
            store
                .get(GROUP_PLEROMA, ":absent_key")
                .await
                .expect("the lookup runs"),
            None,
            "an unregistered pair with no row is nothing at all"
        );
    }

    /// [`RuntimeConfig::set`] merges each write into the stored row, so
    /// a second subkey never drops the first one.
    #[tokio::test]
    async fn set_merges_subkeys_into_the_stored_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let store = RuntimeConfig::new(&pool, &config);

        let first = store
            .set(
                GROUP_PLEROMA,
                KEY_MRF,
                &json!([{"tuple": ["policies", ["simple"]]}]),
            )
            .await
            .expect("the first write runs");
        assert_eq!(
            first,
            json!({"policies": ["simple"]}),
            "the decoded tuple arrives as the stored object"
        );
        let stored = store
            .set(
                GROUP_PLEROMA,
                KEY_MRF_SIMPLE,
                &json!([{"tuple": ["reject", [["evil.example", "bots"]]]}]),
            )
            .await
            .expect("the second write runs");
        assert_eq!(
            stored,
            json!({"reject": [["evil.example", "bots"]]}),
            "the whole :mrf_simple value is stored"
        );
        let rewritten = store
            .set(GROUP_PLEROMA, KEY_MRF, &json!([]))
            .await
            .expect("a plain replace arrives as the empty list");
        assert_eq!(
            rewritten,
            json!({"policies": ["simple"]}),
            "an empty wire list merges nothing away"
        );
    }

    /// [`RuntimeConfig::delete`] drops subkeys, keeps the row while any
    /// remain, and removes it when the last one goes — or wholesale.
    #[tokio::test]
    async fn delete_drops_subkeys_then_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let store = RuntimeConfig::new(&pool, &config);

        assert_eq!(
            store
                .delete(GROUP_PLEROMA, KEY_INSTANCE, None)
                .await
                .expect("the delete runs"),
            DeleteOutcome::Absent,
            "deleting what was never stored changes nothing"
        );

        let wrote = store
            .set(
                GROUP_PLEROMA,
                KEY_INSTANCE,
                &json!([{"tuple": ["name", "A"]}, {"tuple": ["email", "a@b.c"]}]),
            )
            .await
            .expect("the write runs");
        assert_eq!(
            wrote,
            json!({"email": "a@b.c", "name": "A"}),
            "both tuples arrive in the row"
        );
        let outcome = store
            .delete(GROUP_PLEROMA, KEY_INSTANCE, Some(&["name".to_owned()]))
            .await
            .expect("the subkey delete runs");
        assert_eq!(
            outcome,
            DeleteOutcome::Updated(json!({"email": "a@b.c"})),
            "the listed subkey is gone and the row keeps the rest"
        );
        let emptied = store
            .delete(GROUP_PLEROMA, KEY_INSTANCE, Some(&[":email".to_owned()]))
            .await
            .expect("the last subkey delete runs");
        assert_eq!(
            emptied,
            DeleteOutcome::Removed,
            "the emptied row is removed with its last subkey"
        );
        assert_eq!(
            stored(&pool, GROUP_PLEROMA, KEY_INSTANCE)
                .await
                .expect("the lookup runs"),
            None,
            "and the row is really gone"
        );

        let stored_mrf = store
            .set(
                GROUP_PLEROMA,
                KEY_MRF,
                &json!([{"tuple": ["policies", ["noop"]]}]),
            )
            .await
            .expect("the write runs");
        assert_eq!(stored_mrf, json!({"policies": ["noop"]}));
        assert_eq!(
            store
                .delete(GROUP_PLEROMA, KEY_MRF, None)
                .await
                .expect("the whole delete runs"),
            DeleteOutcome::Removed,
            "delete without subkeys takes the whole row"
        );
    }

    /// [`RuntimeConfig::effective_config`] applies the stored `:instance`
    /// overrides to the nine fields their subkeys name, and a
    /// wrong-typed row falls back to env with the rest of the row still
    /// applied.
    #[tokio::test]
    async fn effective_config_overlays_the_instance_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let store = RuntimeConfig::new(&pool, &config);

        let same = store.effective_config().await.expect("the read runs");
        assert_eq!(
            same.instance_name, config.instance_name,
            "no row, no change"
        );

        write(
            &pool,
            GROUP_PLEROMA,
            KEY_INSTANCE,
            &json!({
                "name": "Renamed",
                "description": "Rewritten",
                "email": "staff@example.test",
                "registrations_open": false,
                "account_approval_required": true,
                "account_activation_required": true,
                "invites_enabled": true,
                "limit": 1200,
                "max_media_attachments": 9,
            }),
        )
        .await
        .expect("the row writes");
        let overlaid = store.effective_config().await.expect("the read runs");
        assert_eq!(overlaid.instance_name, "Renamed", "name");
        assert_eq!(overlaid.instance_description, "Rewritten", "description");
        assert_eq!(
            overlaid.contact_email.as_deref(),
            Some("staff@example.test"),
            "email"
        );
        assert!(!overlaid.registrations_open.get(), "registrations_open");
        assert!(
            overlaid.registrations_approval_required.get(),
            "account_approval_required"
        );
        assert!(
            overlaid.account_activation_required.get(),
            "account_activation_required"
        );
        assert!(overlaid.invites_enabled.get(), "invites_enabled");
        assert_eq!(overlaid.max_status_chars, 1200, "limit");
        assert_eq!(overlaid.max_media_attachments, 9, "max_media_attachments");

        write(
            &pool,
            GROUP_PLEROMA,
            KEY_INSTANCE,
            &json!({"name": 42, "invites_enabled": "yes"}),
        )
        .await
        .expect("the broken row writes");
        let fallback = store.effective_config().await.expect("the read runs");
        assert_eq!(
            fallback.instance_name, config.instance_name,
            "a wrong-typed override keeps the env value"
        );
        assert_eq!(
            fallback.invites_enabled.get(),
            config.invites_enabled.get(),
            "and does not poison its siblings"
        );
    }

    /// [`RuntimeConfig::mrf_document`] composes the pipeline document
    /// from the `:mrf` and `:mrf_*` rows, prefers the per-policy rows,
    /// and answers `None` for a store holding none of them.
    #[tokio::test]
    async fn mrf_document_composes_the_mrf_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let store = RuntimeConfig::new(&pool, &config);

        assert_eq!(
            store.mrf_document().await.expect("the read runs"),
            None,
            "an empty store configures nothing"
        );

        write(
            &pool,
            GROUP_PLEROMA,
            KEY_MRF,
            &json!({"policies": ["simple"], "simple": {"reject": ["from-mrf.example"]}}),
        )
        .await
        .expect("the :mrf row writes");
        write(
            &pool,
            GROUP_PLEROMA,
            KEY_MRF_SIMPLE,
            &json!({"reject": ["from-entry.example"]}),
        )
        .await
        .expect("the :mrf_simple row writes");
        write(&pool, GROUP_PLEROMA, ":instance", &json!({"name": "N"}))
            .await
            .expect("a non-mrf row writes");

        let document = store
            .mrf_document()
            .await
            .expect("the read runs")
            .expect("the store answers");
        assert_eq!(
            document,
            json!({
                "policies": ["simple"],
                "simple": {"reject": ["from-entry.example"]},
            }),
            "the :mrf fields compose the document and the dedicated :mrf_* row wins"
        );

        let gone = remove(&pool, GROUP_PLEROMA, KEY_MRF)
            .await
            .expect("the :mrf row goes");
        assert!(gone, "the row was there and is gone");
        let lone = store
            .mrf_document()
            .await
            .expect("the read runs")
            .expect("the store answers");
        assert_eq!(
            lone,
            json!({"simple": {"reject": ["from-entry.example"]}}),
            "a lone policy-params entry still forms a document"
        );
    }
}
