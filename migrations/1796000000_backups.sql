-- Account backups: the export archives the instance has agreed to build for
-- an account — the rows behind the export endpoint and the backup listing.
-- Reference: Pleroma's `Pleroma.User.Backup` (`lib/pleroma/user/backup.ex`)
-- over the same-named table. Here the row is only the record of a request:
-- the archive itself is written by a queue job (docs/ARCHITECTURE.md
-- decision 11), which reads the row, builds the zip and stores it, and marks
-- the row done (decision 3).
CREATE TABLE backups (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- order is the creation order the listings read.
    id uuid PRIMARY KEY,
    -- The owning account. Deleting it deletes their backups, and with them
    -- any request a job has not yet got round to.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The archive's name: `archive-<nickname>-<YYYYMMDDHHMMSS>.zip`, built by
    -- the model so it is also a legal key for the storage layer
    -- (`src/uploads.rs`).
    file_name text NOT NULL,
    -- The archive's size in bytes: 0 until the job has written the file.
    file_size bigint NOT NULL DEFAULT 0,
    -- Whether the archive exists and has been stored — the flag that turns a
    -- download link on.
    processed boolean NOT NULL DEFAULT false,
    -- What the archive is served as; every archive is a zip.
    content_type text NOT NULL DEFAULT 'application/zip',
    -- The storage key the finished archive lives under, NULL until the job
    -- has put it there.
    storage_key text,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Pleroma's schema also carries a `tempdir` column, the directory its worker
-- zips inside and reads back from. That is deliberately not copied: the
-- working directory belongs to the job, not to the record, so it is made when
-- the job runs and removed when the job ends (or dies), and nothing stale has
-- to be cleaned out of this table.

-- One account's backup list, newest first, is the only walk that reads this
-- table by owner; `ORDER BY id DESC` (decision 4) is this index's own order
-- run backwards, and the sweep for the newest single row uses it too.
CREATE INDEX backups_user_id_id_idx ON backups (user_id, id);
