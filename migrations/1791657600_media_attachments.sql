-- Media attachments (docs/ARCHITECTURE.md decision 3: one table per model):
-- files uploaded to this instance and media mirrored from remote statuses.
-- Reference: Pleroma's Pleroma.Activity `attachments` array, flattened into
-- rows Mastodon's Attachment entity renders from
-- (lib/pleroma/web/mastodon_api/views/status_view.ex, "attachment.json").
CREATE TABLE media_attachments (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- Uploading user; NULL for media fetched from the network, and rows
    -- survive their uploader's deletion.
    user_id uuid REFERENCES users(id) ON DELETE SET NULL,
    -- Status this attachment belongs to. A plain uuid for now: the
    -- statuses table arrives in a later ticket, which adds the foreign
    -- key in its own migration. NULL while the media is unposted.
    status_id uuid,
    -- Order within the status's attachment list; 0 for unattached media.
    position integer NOT NULL DEFAULT 0,
    -- Key inside the upload storage (src/uploads.rs) for local files;
    -- NULL for remote media.
    storage_key text,
    -- Original URL of remote media; NULL for local uploads.
    remote_url text,
    -- Public URL clients read: the /media URL of a local file, the
    -- proxied or original URL of a remote one.
    url text NOT NULL,
    -- Preview/thumbnail URL; NULL means clients fall back to `url`.
    preview_url text,
    -- MIME type as stored/detected, e.g. image/png.
    content_type text NOT NULL,
    -- Alt text describing the media for accessibility.
    description text,
    -- Blurhash string for the placeholder preview.
    blurhash text,
    -- Dimensions of the original media in pixels.
    width integer,
    height integer,
    -- Length of audio and video in seconds.
    duration double precision,
    -- Focal point for cropping, each axis in [-1.0, 1.0].
    focus_x real,
    focus_y real,
    -- Size of the local file in bytes.
    file_size bigint,
    -- Uploader flagged the media as sensitive (Mastodon's `sensitive`).
    sensitive_hint boolean NOT NULL DEFAULT false,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Attachment lookups: everything a user uploaded, and everything a status
-- carries.
CREATE INDEX media_attachments_user_id_idx ON media_attachments (user_id);
CREATE INDEX media_attachments_status_id_idx ON media_attachments (status_id);
