-- Status-creation idempotency keys: the `(account, key)` → status mapping
-- behind the `Idempotency-Key` header of `POST /api/v1/statuses`. A retry of
-- a create whose key is already stored answers the status the key made
-- instead of posting it twice.
-- Reference: Pleroma's `Pleroma.Web.Plugs.IdempotencyPlug`
-- (`lib/pleroma/web/plugs/idempotency_plug.ex`), whose Cachex cache this
-- table replaces with a durable one (docs/ARCHITECTURE.md decision 11 keeps
-- this instance's shared state in Postgres); the 24-hour horizon of Pleroma's
-- `:idempotency_ttl` is what the `idempotency.cleanup` sweep enforces here.
CREATE TABLE idempotency_keys (
    -- The account that sent the key: keys are scoped per user, so two
    -- accounts may use the same key text for two different posts. Deleting
    -- the user retires their keys with it.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The `Idempotency-Key` header value verbatim; the API ignores longer
    -- spellings, so this column never holds more than 255 characters.
    key text NOT NULL,
    -- The status this key created. Deleting the row (the hard delete an
    -- account purge performs) retires the key with it, so the pair is free
    -- again.
    status_id uuid NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One status per account and key: the pair the create's
    -- `ON CONFLICT DO NOTHING` inserts against, and the re-read looks up by.
    PRIMARY KEY (user_id, key)
);

-- The sweep's walk: delete every key older than the retention horizon.
CREATE INDEX idempotency_keys_inserted_at_idx ON idempotency_keys (inserted_at);
