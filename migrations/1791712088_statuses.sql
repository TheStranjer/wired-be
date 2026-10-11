-- Statuses: posts, replies, reblogs and quotes (docs/ARCHITECTURE.md
-- decision 6: relational Mastodon-style rows, not Pleroma's JSONB
-- activities). Reference: Pleroma stores the same facts in
-- `Pleroma.Activity` wrapping an `Object` (lib/pleroma/activity.ex,
-- lib/pleroma/object.ex) — different storage, same concepts: a
-- `Create`/`Announce` activity becomes one row here, its actor becomes
-- `user_id` and its `published` time becomes `inserted_at`.
CREATE TABLE statuses (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- ActivityPub object id (decision 6): `{BASE_URL}/objects/{uuid}` for a
    -- status this instance owns, whatever a remote instance sent for a
    -- mirrored one. Unique, and kept after a soft delete, so a deleted
    -- remote post is never re-imported.
    uri text NOT NULL UNIQUE,
    -- Human-facing URL of the post (`{BASE_URL}/notice/{uuid}` locally, the
    -- remote's `url` property elsewhere); NULL when the origin gave none.
    url text,
    -- Author. Deleting the account takes its statuses with it, and a
    -- reblog of a removed status goes the same way.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The post as sanitized HTML (src/util/sanitize.rs); emptied by a soft
    -- delete so a tombstone keeps no text.
    content text NOT NULL DEFAULT '',
    -- The post as its source text, before formatting.
    raw_text text NOT NULL DEFAULT '',
    -- MIME type `raw_text` was written in: text/plain, text/markdown,
    -- text/html, ...
    content_type text NOT NULL DEFAULT 'text/plain',
    -- Content warning subject line (Mastodon's `spoiler_text`).
    spoiler_text text NOT NULL DEFAULT '',
    -- Whether `spoiler_text` hides `content` until the reader opens it.
    sensitive boolean NOT NULL DEFAULT false,
    -- Who may see the post (src/models/status.rs, `Visibility`); `local` is
    -- this instance's Pleroma extension meaning "never federated".
    visibility text NOT NULL
        CONSTRAINT statuses_visibility_check
        CHECK (visibility IN ('public', 'unlisted', 'private', 'direct', 'local')),
    -- BCP 47 language tag of the content, when known.
    language text,
    -- Status this one replies to; a deleted parent leaves the reply in
    -- place with its answer count gone.
    in_reply_to_id uuid REFERENCES statuses(id) ON DELETE SET NULL,
    -- Author of the replied-to status, when that account is known here.
    in_reply_to_account_id uuid REFERENCES users(id) ON DELETE SET NULL,
    -- ActivityPub id of the replied-to status when it is not (yet) stored:
    -- a remote reply whose target has not been fetched.
    in_reply_to_ap_id text,
    -- Thread group id: the root status of a conversation is its own id,
    -- every reply inherits it (Mastodon's `conversation`).
    conversation_id uuid NOT NULL,
    -- Original this status reblogs: a reblog is a row of its own carrying
    -- no text (decision 6). Deleting the original removes the reblogs.
    reblog_of_id uuid REFERENCES statuses(id) ON DELETE CASCADE,
    -- Original this status quotes, if any; a removed original leaves the
    -- quoting status with its quote gone.
    quote_of_id uuid REFERENCES statuses(id) ON DELETE SET NULL,
    -- Whether this instance owns the post, as in `users.local`.
    local boolean NOT NULL,
    -- Client that posted this status, kept for the entity's `application`
    -- block (name and URL, not a foreign key to `oauth_apps`).
    application_name text,
    application_website text,
    -- Denormalised counters, moved by src/models/status.rs only, and never
    -- below zero.
    replies_count integer NOT NULL DEFAULT 0,
    reblogs_count integer NOT NULL DEFAULT 0,
    favourites_count integer NOT NULL DEFAULT 0,
    quotes_count integer NOT NULL DEFAULT 0,
    -- When the content last changed (an Edit activity); NULL until edited.
    edited_at timestamptz,
    -- Soft delete tombstone: the row and its `uri` stay so a deleted remote
    -- post is not re-imported, while its text is cleared. NULL means live.
    deleted_at timestamptz,
    -- Scheduled deletion time (Mastodon's `expires_at`); NULL means the
    -- post never expires.
    expires_at timestamptz,
    -- `inserted_at` is the status's creation time: for a remote status the
    -- `published` timestamp it arrived with.
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- One account's posting history, newest first, live rows only — the
-- partial index timelines and `GET /api/v1/accounts/{id}/statuses` read.
CREATE INDEX statuses_user_id_idx ON statuses (user_id, id DESC)
    WHERE deleted_at IS NULL;
-- A status's answers, and the threads it belongs to.
CREATE INDEX statuses_in_reply_to_id_idx ON statuses (in_reply_to_id);
CREATE INDEX statuses_conversation_id_idx ON statuses (conversation_id);
-- The reblogs of an original, and its quotes.
CREATE INDEX statuses_reblog_of_id_idx ON statuses (reblog_of_id);
CREATE INDEX statuses_quote_of_id_idx ON statuses (quote_of_id);
-- Public timelines: the newest live post of a visibility.
CREATE INDEX statuses_visibility_idx ON statuses (visibility, id DESC)
    WHERE deleted_at IS NULL;

-- `media_attachments.status_id` was a plain uuid until this table existed
-- (see migrations/1791657600_media_attachments.sql); now media belongs to a
-- status properly, and deleting a status — hard-deleting one, since a soft
-- delete keeps the row — takes its attachments with it.
ALTER TABLE media_attachments
    ADD CONSTRAINT media_attachments_status_id_fkey
    FOREIGN KEY (status_id) REFERENCES statuses(id) ON DELETE CASCADE;
