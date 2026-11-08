-- Instance secrets (WBE-144): the one place secret instance-wide material
-- is kept, so the Web Push sender can read the VAPID key pair it signs
-- `Authorization` headers with. Reference: Pleroma keeps its VAPID pair in
-- runtime config (`config/runtime.exs`, the `:web_push_encryption,
-- :vapid_details` keyword); here each value is a row, generated on first
-- use by `wired-be`'s startup (see `crate::services::vapid`) and
-- overridable with `VAPID_PRIVATE_KEY` / `VAPID_PUBLIC_KEY`. The rows this
-- ticket writes are `vapid_private` (the ES256 signing key as PKCS#8 PEM)
-- and `vapid_public` (the public point, URL-safe base64 of its uncompressed
-- SEC1 encoding); later tickets may add more names to the same table.
CREATE TABLE instance_secrets (
    -- Stable name of one secret, e.g. `vapid_private`.
    name text PRIMARY KEY,
    -- Its value. Never served over HTTP and never logged; the readers that
    -- hand a value to a log redact it first.
    value text NOT NULL
);
