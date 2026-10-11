-- Announcements: instance-wide notices, and who has dismissed which.
-- Reference: Pleroma's `Pleroma.Announcement` (lib/pleroma/announcement.ex),
-- which keeps the text in two JSONB maps (`data` for the source the author
-- typed, `rendered` for the HTML clients see) plus the activity window, and
-- `Pleroma.AnnouncementReadRelationship`
-- (lib/pleroma/announcement_read_relationship.ex) for dismissals. Per
-- docs/ARCHITECTURE.md decision 6 those maps become columns here: `content`
-- holds what `rendered["content"]` held, `raw_content` what `data["content"]`
-- did, and a dismissal is one row of the join table instead of a relationship.
CREATE TABLE announcements (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- sorts in creation order, so "newest first" is a plain `ORDER BY id DESC`
    -- and the id doubles as the announcement's insertion stamp.
    id uuid PRIMARY KEY,
    -- The notice as sanitized HTML (`src/util/sanitize.rs`), which is what
    -- clients render. NOT NULL: an empty notice is the empty string, not a
    -- missing one.
    content text NOT NULL,
    -- The same notice as its author wrote it, before sanitizing: Pleroma
    -- serves it to clients in `pleroma.raw_content`.
    raw_content text NOT NULL,
    -- Start of the activity window; NULL means the notice is active before any
    -- time one could name (Pleroma's `starts_at IS NULL OR starts_at < now()`).
    starts_at timestamptz,
    -- End of the activity window; NULL means it never stops being active.
    ends_at timestamptz,
    -- Whether clients show the window's dates without their times.
    all_day boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Who dismissed which notice, one row per pair: Pleroma's
-- `announcement_read_relationships`, whose unique `(user_id, announcement_id)`
-- constraint is the primary key here. A dismissal is per viewer, so nobody
-- hides a notice from anybody else, and `mark_read` is idempotent on this key.
-- The announcements list runs its read flag off a lookup by
-- `(announcement_id, user_id)`, which the primary key serves directly;
-- announcements are few, so scanning the table whole needs no index of its
-- own.
CREATE TABLE announcement_reads (
    -- Deleting the notice takes its dismissals with it, which is what keeps a
    -- re-created notice from arriving already-read.
    announcement_id uuid NOT NULL REFERENCES announcements(id) ON DELETE CASCADE,
    -- Deleting the account forgets everything they dismissed.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- When they dismissed it.
    inserted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (announcement_id, user_id)
);

-- Everything one account has dismissed: the reverse direction of the key, and
-- what the cascade above walks when an account is deleted.
CREATE INDEX announcement_reads_user_id_idx ON announcement_reads (user_id);
