-- Instance settings (WBE-166): the tiny key/value table behind
-- instance-wide settings the admin can change at runtime, starting with
-- `frontend_configurations` (Pleroma keeps that in its runtime `Config`;
-- here it is a row). `src/models/instance_setting.rs` owns the queries;
-- the admin configuration ticket reuses the table for the other settings.
CREATE TABLE instance_settings (
    -- The setting's name, e.g. `frontend_configurations`. The primary key
    -- is what makes [`set`](wired_be::models::instance_setting::set) an
    -- upsert instead of an insert.
    key text PRIMARY KEY,
    -- The setting's value as JSON: shapes are the readers' business, the
    -- column only says it must be there.
    value jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
