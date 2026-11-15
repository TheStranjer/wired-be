-- Admin webhooks (WBE-209): the HTTP endpoints that event deliveries are
-- POSTed to, one row per webhook. Reference: Pleroma's `Pleroma.Webhook`
-- (`lib/pleroma/webhook.ex`) and its admin API `WebhookController`
-- (`lib/pleroma/web/admin_api/controllers/webhook_controller.ex`).
-- `src/models/webhook.rs` owns the queries and
-- `src/api/admin/webhooks.rs` the CRUD/enable/disable/rotate_secret routes.
CREATE TABLE webhooks (
    -- The `UUIDv7` primary key, minted in Rust (decision 4); the admin
    -- listing orders on it, so it answers in creation order.
    id uuid PRIMARY KEY,
    -- Where events are delivered: an `http(s)` URL, validated by the API.
    url text NOT NULL,
    -- The event types this webhook subscribes to (known: `report.created`,
    -- `report.updated`, `account.created`; the API refuses any other).
    events text[] NOT NULL DEFAULT '{}',
    -- The signing secret the delivery HMACs its payloads with: 32 random
    -- bytes as lowercase hex, minted on create and changed only by
    -- `POST /webhooks/{id}/rotate_secret`. Secret material, redacted from
    -- `Debug`.
    secret text NOT NULL,
    -- Whether deliveries happen at all; the enable/disable routes flip it.
    enabled boolean NOT NULL DEFAULT true,
    -- Whether the instance itself installed this webhook. Internal
    -- webhooks ride along in every read but the admin API refuses to
    -- change them (delete 422s, the other writes 403).
    internal boolean NOT NULL DEFAULT false,
    -- Creation and last-change stamps; the update queries raise
    -- `updated_at` with `now()`.
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
