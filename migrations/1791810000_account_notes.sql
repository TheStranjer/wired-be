-- Account notes: one account's private note on another account, the data
-- behind the `note` flag of the Relationship entity. Reference: Pleroma's
-- UserNote schema (lib/pleroma/user_note.ex) — its `user_notes` table keyed
-- on (source_id, target_id); the ticket names the pair here
-- (user_id, target_id).
CREATE TABLE account_notes (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The author of the note. Deleting the account deletes their notes.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The account the note is about. Deleting it removes every note on it.
    target_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The note text. A blank note is not stored: the model deletes the row
    -- on an empty write (Pleroma's `UserNote.show` answers "" for a pair
    -- without one, so blank and absent are the same reading state).
    comment text NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- One note per ordered pair: the key set() is idempotent on. Unlike
    -- follows, a note on oneself is allowed, so no self-check constraint.
    CONSTRAINT account_notes_user_id_target_id_key UNIQUE (user_id, target_id)
);

-- Every read here is one viewer's notes over a batch of targets, which the
-- unique constraint's index already serves; the reverse (target) side is
-- only ever touched by the cascade, which starts from the deleted user row.
