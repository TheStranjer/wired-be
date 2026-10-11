-- Filters: per-user phrase filters with the Mastodon v1 semantics
-- (`docs/ARCHITECTURE.md` WBE-98). Reference: Pleroma's `Pleroma.Filter`
-- (lib/pleroma/filter.ex); its `filter_id` per-user counter and Oban
-- purge job have no place here — the id is this row's `UUIDv7` (decision 4)
-- and expiry is a query condition, so no cleanup job is needed — and its
-- `hide` column carries the API name `irreversible` (Pleroma's controller
-- maps `irreversible` onto it).
CREATE TABLE filters (
    -- UUIDv7 made in Rust (decision 4); no database-side default. Its text
    -- sorts in creation order, so the newest-first list is a plain
    -- `ORDER BY id DESC`.
    id uuid PRIMARY KEY,
    -- The filter belongs to exactly one account, and an account's filters
    -- go with it: Pleroma's user-scoped `get_filters`/`get` are the
    -- ownership boundary, enforced here by the cascade.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The phrase to look for. NOT NULL; emptiness is an API validation
    -- error, not a missing column (Pleroma's `validate_required`).
    phrase text NOT NULL,
    -- Where the filter applies: a subset of
    -- `home`, `notifications`, `public`, `thread`, `account`. The API
    -- validates the values and that the list is non-empty; the column
    -- stores exactly what passed.
    context text[] NOT NULL,
    -- Whether the phrase must appear as a whole word (Pleroma's default
    -- `true`, the column default here).
    whole_word boolean NOT NULL DEFAULT true,
    -- Whether clients hide matching posts instead of warning
    -- (Mastodon's `irreversible`, Pleroma's `hide`).
    irreversible boolean NOT NULL DEFAULT false,
    -- When the filter stops applying; NULL means it never does
    -- (Pleroma's `expires_at`, set from `expires_in` seconds).
    expires_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Everything one account has, and the per-user ownership lookups: both the
-- list query and `WHERE id = … AND user_id = …` guards start here.
CREATE INDEX filters_user_id_idx ON filters (user_id);
