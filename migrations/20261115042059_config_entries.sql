-- Admin runtime configuration store (WBE-206): the `config_entries`
-- table behind `GET/POST /api/v1/pleroma/admin/config` (Pleroma's
-- `config` table, `lib/pleroma/config_db.ex`). One row per whitelisted
-- (group, key) pair — `:pleroma`/`:instance` and the `:mrf*` families —
-- holding that key's DB-side overrides as JSON. `src/services/runtime_config.rs`
-- owns the queries; only keys the registry whitelists are changeable at
-- runtime, and reads fall back to the env-derived `Config`.
CREATE TABLE config_entries (
    -- The configuration group, colon-spelled as the admin API names it:
    -- `:pleroma` for every entry this ticket registers. `group_name`
    -- because `group` is reserved in PostgreSQL.
    group_name text NOT NULL,
    -- The configuration key, colon-spelled: `:instance`, `:mrf`,
    -- `:mrf_simple`, …
    key text NOT NULL,
    -- The key's value as JSON: an object of subkeys for keyword-list
    -- settings (the API renders them as `{"tuple": […]}` arrays), plain
    -- JSON for everything else.
    value jsonb NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- The pair is the key: one row per setting, and the composite primary
    -- key is what makes the store's write an upsert.
    PRIMARY KEY (group_name, key)
);
