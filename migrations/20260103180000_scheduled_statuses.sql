-- Scheduled statuses: a create-status request parked until a future moment,
-- Pleroma's `Pleroma.ScheduledActivity` (lib/pleroma/scheduled_activity.ex).
-- The row is the whole request kept for replay: `params` holds the
-- normalised create-status parameters as they were received, and when
-- `scheduled_at` arrives a worker rebuilds the status from them through the
-- ordinary create path and removes the row. Nothing is written to
-- `statuses` or `media_attachments` before the fire; an attachment named in
-- `params` is reserved by this row's reference alone, so a pending schedule
-- keeps the media from being posted by anything else (src/models/scheduled_status.rs).
CREATE TABLE scheduled_statuses (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The account the post is scheduled for. Deleting the account takes its
    -- pending schedules with them: a schedule of a gone account never fires.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The earliest moment the status should fire (decision 5: timestamptz,
    -- DateTime<Utc> in Rust). Mastodon's `scheduled_at` request parameter,
    -- held in a column rather than in `params` because the database reads
    -- it: the due sweep below and the per-user listings both order by it.
    scheduled_at timestamptz NOT NULL,
    -- The normalised create-status params as received: the request keys
    -- `status`, `visibility`, `sensitive`, `spoiler_text`, `language`,
    -- `content_type`, `expires_in`, `media_ids` (an array of id strings)
    -- and `in_reply_to_id`. Not `scheduled_at` — that lives in the column
    -- above — and not `preview`, which is not stored.
    params jsonb NOT NULL,
    -- When the schedule was created (Mastodon's `inserted_at` entity field).
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- When the row last changed; a schedule is written once and then only
    -- read or deleted, so this stays equal to `inserted_at` in practice.
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- One account's schedules in due order: the listings (`GET
-- /api/v1/scheduled_statuses` and its per-user count) and the per-UTC-day
-- cap check walk (user_id, scheduled_at) as a half-open range.
CREATE INDEX scheduled_statuses_user_id_scheduled_at_idx
    ON scheduled_statuses (user_id, scheduled_at);
-- The worker's sweep: everything due by now, oldest first, a bounded page.
CREATE INDEX scheduled_statuses_scheduled_at_idx
    ON scheduled_statuses (scheduled_at);
